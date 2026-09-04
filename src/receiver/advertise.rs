use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use sha2::{Digest, Sha256};

use crate::discovery::DeviceCapability;

const CAST_SERVICE: &str = "_googlecast._tcp.local.";

pub struct AdvertiseOptions {
    /// Stable receiver UUID (no dashes) used in the `id` TXT record.
    pub receiver_id: String,
    pub friendly_name: String,
    pub model: String,
    pub capability: DeviceCapability,
    pub port: u16,
}

/// An advertised `_googlecast._tcp` service backed by an mDNS daemon.
pub struct AdvertisedService {
    daemon: ServiceDaemon,
    fullname: String,
}

impl AdvertisedService {
    /// Starts advertising the receiver; the daemon announces on all
    /// interfaces, re-announcing automatically if interface addresses change.
    pub fn start(options: AdvertiseOptions) -> Result<Self> {
        let daemon =
            ServiceDaemon::new().context("could not start the mDNS responder for the receiver")?;
        let properties = txt_properties(&options);
        let instance = options.friendly_name.clone();
        let host_name = host_name(&options.friendly_name);
        let info = ServiceInfo::new(
            CAST_SERVICE,
            &instance,
            &host_name,
            "", // addr_auto fills in addresses from every interface.
            options.port,
            &properties[..],
        )
        .context("could not prepare the receiver mDNS service")?
        .enable_addr_auto();
        daemon
            .register(info)
            .context("could not advertise the receiver on the local network")?;
        Ok(Self {
            daemon,
            fullname: format!("{instance}.{CAST_SERVICE}"),
        })
    }

    /// Unregisters the service and shuts the mDNS responder down, announcing
    /// the goodbye packets senders expect.
    pub fn stop(self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Builds the TXT record set for the advertisement. `id` is mandatory for
/// pychromecast-style senders; `fn`, `md`, and `ca` are what our own
/// discovery path reads.
pub fn txt_properties(options: &AdvertiseOptions) -> Vec<(String, String)> {
    let capabilities = match options.capability {
        DeviceCapability::Video => "5",
        DeviceCapability::AudioOnly => "4",
        DeviceCapability::Unknown => "5",
    };
    let mut records = vec![
        ("id".to_owned(), options.receiver_id.clone()),
        ("cd".to_owned(), cd_hash(&options.receiver_id)),
        ("fn".to_owned(), options.friendly_name.clone()),
        ("md".to_owned(), options.model.clone()),
        ("ca".to_owned(), capabilities.to_owned()),
        ("ve".to_owned(), "02".to_owned()),
        ("st".to_owned(), "0".to_owned()),
        ("rm".to_owned(), String::new()),
        ("bs".to_owned(), random_hex(8)),
        ("ic".to_owned(), String::new()),
    ];
    records.shrink_to_fit();
    records
}

/// The `cd` record mirrors real Chromecasts: a stable hash derived from the
/// receiver id rather than a second random UUID.
fn cd_hash(receiver_id: &str) -> String {
    Sha256::digest(receiver_id.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..32]
        .to_owned()
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer).expect("randomness is always available");
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The mDNS host name: the friendly name reduced to a valid DNS label.
pub fn host_name(friendly_name: &str) -> String {
    let mut sanitized: String = friendly_name
        .chars()
        .map(|character| {
            if char::is_ascii_alphanumeric(&character.to_ascii_lowercase()) {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    sanitized = sanitized
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if sanitized.is_empty() {
        sanitized = "cast-receiver".to_owned();
    }
    format!("{sanitized}.local.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(name: &str, capability: DeviceCapability) -> AdvertiseOptions {
        AdvertiseOptions {
            receiver_id: "0123456789abcdef0123456789abcdef".to_owned(),
            friendly_name: name.to_owned(),
            model: "Cast Desktop Receiver".to_owned(),
            capability,
            port: 8009,
        }
    }

    fn record<'a>(records: &'a [(String, String)], key: &str) -> &'a str {
        records
            .iter()
            .find(|(key_, _)| key_ == key)
            .map(|(_, value)| value.as_str())
            .unwrap_or_else(|| panic!("missing {key} record"))
    }

    #[test]
    fn txt_records_expose_the_fields_senders_require() {
        let records = txt_properties(&options("Living Room Cast", DeviceCapability::Video));
        assert_eq!(record(&records, "id"), "0123456789abcdef0123456789abcdef");
        assert_eq!(record(&records, "fn"), "Living Room Cast");
        assert_eq!(record(&records, "md"), "Cast Desktop Receiver");
        assert_eq!(record(&records, "ca"), "5");
        assert_eq!(record(&records, "ve"), "02");
        assert_eq!(record(&records, "st"), "0");
        assert_eq!(record(&records, "ic"), "");
        assert_eq!(record(&records, "bs").len(), 16);
    }

    #[test]
    fn audio_only_receivers_advertise_the_audio_capability_bit() {
        let records = txt_properties(&options("Bedroom", DeviceCapability::AudioOnly));
        assert_eq!(record(&records, "ca"), "4");
    }

    #[test]
    fn cd_record_is_a_stable_hash_of_the_receiver_id() {
        let records = txt_properties(&options("Test", DeviceCapability::Video));
        let again = txt_properties(&options("Test", DeviceCapability::AudioOnly));
        assert_eq!(record(&records, "cd"), record(&again, "cd"));
        assert_eq!(record(&records, "cd").len(), 32);
    }

    #[test]
    fn host_names_are_sanitized_to_valid_local_names() {
        assert_eq!(host_name("Living Room Cast"), "living-room-cast.local.");
        assert_eq!(host_name("Pïñk-Dolphin!!"), "p-k-dolphin.local.");
        assert_eq!(host_name("---"), "cast-receiver.local.");
        assert_eq!(host_name(""), "cast-receiver.local.");
    }
}
