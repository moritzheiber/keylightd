use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::config::LightConfig;

const SERVICE_TYPE: &str = "_elg._tcp.local.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightState {
    pub on: bool,
    pub brightness: u8,
}

pub struct KeyLight {
    endpoint: String,
    client: Client,
}

#[derive(Deserialize)]
struct LightsResponse {
    lights: Vec<ApiLight>,
}

#[derive(Deserialize, Serialize)]
struct ApiLight {
    on: u8,
    brightness: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LightsUpdate {
    number_of_lights: u8,
    lights: Vec<ApiLight>,
}

impl KeyLight {
    pub fn connect(config: &LightConfig) -> Result<Self> {
        let endpoint = match discover(config.discovery_timeout_seconds) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                let Some(address) = config.address.as_deref() else {
                    return Err(error).context(
                        "Key Light mDNS discovery failed; set light.address to its IP or host:port",
                    );
                };
                debug!(%error, address, "using configured Key Light address");
                normalize_endpoint(address)
            }
        };
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(3))
            .build()
            .context("build Key Light HTTP client")?;
        Ok(Self { endpoint, client })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn state(&self) -> Result<LightState> {
        let response = self
            .client
            .get(format!("{}/elgato/lights", self.endpoint))
            .send()
            .context("query Key Light")?
            .error_for_status()
            .context("Key Light returned an error")?
            .json::<LightsResponse>()
            .context("decode Key Light response")?;
        let light = response
            .lights
            .first()
            .context("Key Light response is empty")?;
        Ok(LightState {
            on: light.on != 0,
            brightness: light.brightness,
        })
    }

    pub fn set_power_brightness(&self, on: bool, brightness: u8) -> Result<()> {
        if !(1..=100).contains(&brightness) {
            bail!("brightness must be between 1 and 100");
        }
        let update = LightsUpdate {
            number_of_lights: 1,
            lights: vec![ApiLight {
                on: u8::from(on),
                brightness,
            }],
        };
        self.client
            .put(format!("{}/elgato/lights", self.endpoint))
            .json(&update)
            .send()
            .context("update Key Light")?
            .error_for_status()
            .context("Key Light returned an error")?;
        info!(
            on,
            brightness,
            endpoint = self.endpoint,
            "updated Key Light"
        );
        Ok(())
    }
}

fn discover(timeout_seconds: u64) -> Result<String> {
    let daemon = ServiceDaemon::new().context("start mDNS discovery")?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .context("browse for Key Light")?;
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let result = loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break None;
        };
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let address = preferred_address(info.get_addresses());
                if let Some(address) = address {
                    break Some(format!("http://{address}:{}", info.get_port()));
                }
            }
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    daemon.shutdown().context("stop mDNS discovery")?;
    result.context("no Elgato Key Light found through mDNS")
}

fn preferred_address(addresses: &std::collections::HashSet<ScopedIp>) -> Option<IpAddr> {
    addresses
        .iter()
        .find(|address| address.is_ipv4())
        .or_else(|| addresses.iter().next())
        .map(ScopedIp::to_ip_addr)
}

fn normalize_endpoint(address: &str) -> String {
    if address.starts_with("http://") || address.starts_with("https://") {
        address.trim_end_matches('/').to_owned()
    } else if address.contains(':') {
        format!("http://{}", address.trim_end_matches('/'))
    } else {
        format!("http://{}:9123", address.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_configured_addresses() {
        assert_eq!(normalize_endpoint("192.0.2.1"), "http://192.0.2.1:9123");
        assert_eq!(
            normalize_endpoint("192.0.2.1:9123"),
            "http://192.0.2.1:9123"
        );
        assert_eq!(
            normalize_endpoint("http://keylight.local:9123/"),
            "http://keylight.local:9123"
        );
    }
}
