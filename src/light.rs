use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent};
use serde::{Deserialize, Serialize};
use tracing::info;
use ureq::Agent;

use crate::config::{LightConfig, SelectedLight};
use crate::domain::{LogicalLightState, MIRED_MAX, MIRED_MIN};

const SERVICE_TYPE: &str = "_elg._tcp.local.";

#[derive(Clone, Debug)]
pub struct DiscoveredLight {
    pub id: String,
    pub name: String,
    pub service_name: Option<String>,
    pub endpoint: String,
}

pub struct KeyLight {
    discovered: DiscoveredLight,
    client: Agent,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessoryInfo {
    product_name: String,
    serial_number: String,
    mac_address: String,
    display_name: String,
}

#[derive(Deserialize)]
struct LightsResponse {
    lights: Vec<ApiLight>,
}

#[derive(Clone, Deserialize, Serialize)]
struct ApiLight {
    on: u8,
    brightness: u8,
    temperature: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LightsUpdate {
    number_of_lights: usize,
    lights: Vec<ApiLight>,
}

impl KeyLight {
    pub fn connect(discovered: DiscoveredLight) -> Result<Self> {
        let client = http_client();
        Ok(Self { discovered, client })
    }

    pub fn discovered(&self) -> &DiscoveredLight {
        &self.discovered
    }

    pub fn states(&self) -> Result<Vec<LogicalLightState>> {
        let response = self
            .client
            .get(format!("{}/elgato/lights", self.discovered.endpoint))
            .call()
            .with_context(|| format!("query Key Light {}", self.discovered.id))?
            .body_mut()
            .read_json::<LightsResponse>()
            .context("decode Key Light response")?;
        if response.lights.is_empty() {
            bail!(
                "Key Light {} returned no logical lights",
                self.discovered.id
            );
        }
        Ok(response
            .lights
            .into_iter()
            .map(|light| LogicalLightState {
                on: light.on != 0,
                brightness: light.brightness,
                temperature: light.temperature,
            })
            .collect())
    }

    pub fn set_states(&self, states: &[LogicalLightState]) -> Result<()> {
        if states.is_empty() {
            bail!("cannot apply an empty logical light state");
        }
        if states
            .iter()
            .any(|state| !(1..=100).contains(&state.brightness))
        {
            bail!("brightness must be between 1 and 100");
        }
        if states
            .iter()
            .any(|state| !(MIRED_MIN..=MIRED_MAX).contains(&state.temperature))
        {
            bail!("colour temperature must be between {MIRED_MIN} and {MIRED_MAX} mired");
        }
        let update = LightsUpdate {
            number_of_lights: states.len(),
            lights: states
                .iter()
                .map(|state| ApiLight {
                    on: u8::from(state.on),
                    brightness: state.brightness,
                    temperature: state.temperature,
                })
                .collect(),
        };
        self.client
            .put(format!("{}/elgato/lights", self.discovered.endpoint))
            .send_json(&update)
            .with_context(|| format!("update Key Light {}", self.discovered.id))?;
        info!(
            light_id = self.discovered.id,
            logical_lights = states.len(),
            "updated Key Light"
        );
        Ok(())
    }
}

pub fn discover_all(timeout_seconds: u64) -> Result<Vec<DiscoveredLight>> {
    let daemon = ServiceDaemon::new().context("start mDNS discovery")?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .context("browse for Key Lights")?;
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let client = http_client();
    let mut endpoints = BTreeMap::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(address) = preferred_address(info.get_addresses()) {
                    endpoints.insert(
                        info.get_fullname().to_owned(),
                        format!("http://{address}:{}", info.get_port()),
                    );
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    daemon.shutdown().context("stop mDNS discovery")?;

    let mut lights = Vec::new();
    let mut identities = HashSet::new();
    for (service_name, endpoint) in endpoints {
        match identify_endpoint(&client, &endpoint, Some(service_name)) {
            Ok(light) if identities.insert(light.id.clone()) => lights.push(light),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, endpoint, "ignored unidentified Elgato service");
            }
        }
    }
    lights.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    Ok(lights)
}

pub fn resolve_selected(
    config: &LightConfig,
    selected: &[SelectedLight],
) -> Vec<(SelectedLight, Result<KeyLight>)> {
    if selected.is_empty() {
        return Vec::new();
    }
    let client = http_client();
    let mut results = Vec::new();
    let mut unresolved = Vec::new();
    for selection in selected.iter().cloned() {
        let resolved = selection
            .fallback_address
            .as_deref()
            .map(|address| {
                let endpoint = normalize_endpoint(address);
                let light = identify_endpoint(&client, &endpoint, selection.service_name.clone())?;
                if light.id != selection.id {
                    bail!(
                        "fallback for {} resolved to different light {}",
                        selection.id,
                        light.id
                    );
                }
                KeyLight::connect(light)
            })
            .transpose();
        match resolved {
            Ok(Some(light)) => results.push((selection, Ok(light))),
            Ok(None) => unresolved.push(selection),
            Err(error) => {
                tracing::debug!(
                    %error,
                    light_id = selection.id,
                    "Key Light fallback failed; trying mDNS"
                );
                unresolved.push(selection);
            }
        }
    }
    if unresolved.is_empty() {
        return results;
    }
    let discovered = discover_all(config.discovery_timeout_seconds).unwrap_or_else(|error| {
        tracing::warn!(%error, "Key Light discovery failed");
        Vec::new()
    });
    results.extend(unresolved.into_iter().map(|selection| {
        let resolved = discovered
            .iter()
            .find(|light| light.id == selection.id)
            .cloned()
            .map(KeyLight::connect)
            .unwrap_or_else(|| {
                Err(anyhow::anyhow!(
                    "selected Key Light {} is unreachable",
                    selection.id
                ))
            });
        (selection, resolved)
    }));
    results.sort_by_key(|(selection, _)| {
        selected
            .iter()
            .position(|configured| configured.id == selection.id)
            .unwrap_or(usize::MAX)
    });
    results
}

pub fn selected_from_discovered(light: &DiscoveredLight) -> SelectedLight {
    SelectedLight {
        id: light.id.clone(),
        name: light.name.clone(),
        service_name: light.service_name.clone(),
        fallback_address: Some(light.endpoint.clone()),
    }
}

fn identify_endpoint(
    client: &Agent,
    endpoint: &str,
    service_name: Option<String>,
) -> Result<DiscoveredLight> {
    let info = client
        .get(format!("{endpoint}/elgato/accessory-info"))
        .call()
        .with_context(|| format!("query accessory info at {endpoint}"))?
        .body_mut()
        .read_json::<AccessoryInfo>()
        .context("decode accessory-info")?;
    let id = if !info.serial_number.trim().is_empty() {
        format!("serial:{}", info.serial_number)
    } else if !info.mac_address.trim().is_empty() {
        format!("mac:{}", info.mac_address.to_ascii_uppercase())
    } else {
        bail!("accessory-info at {endpoint} has no stable hardware identity");
    };
    let name = display_name(info.product_name, info.display_name);
    Ok(DiscoveredLight {
        id,
        name,
        service_name,
        endpoint: endpoint.to_owned(),
    })
}

/// The device's user-facing name is the mobile-app display name when set, and
/// the product name otherwise.
fn display_name(product_name: String, display_name: String) -> String {
    if display_name.trim().is_empty() {
        product_name
    } else {
        display_name
    }
}

fn http_client() -> Agent {
    Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into()
}

fn preferred_address(addresses: &HashSet<ScopedIp>) -> Option<IpAddr> {
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
    fn display_name_prefers_mobile_app_name() {
        assert_eq!(
            display_name("Elgato Key Light".to_owned(), "Desk".to_owned()),
            "Desk"
        );
    }

    #[test]
    fn display_name_falls_back_to_product_when_blank() {
        assert_eq!(
            display_name("Elgato Key Light".to_owned(), "   ".to_owned()),
            "Elgato Key Light"
        );
    }

    #[test]
    fn normalizes_configured_addresses() {
        assert_eq!(normalize_endpoint("192.0.2.1"), "http://192.0.2.1:9123");
        assert_eq!(
            normalize_endpoint("192.0.2.1:9123"),
            "http://192.0.2.1:9123"
        );
    }

    #[test]
    fn selected_light_retains_stable_identity_and_resolution_hints() {
        let selected = selected_from_discovered(&DiscoveredLight {
            id: "serial:one".to_owned(),
            name: "Desk".to_owned(),
            service_name: Some("Desk._elg._tcp.local.".to_owned()),
            endpoint: "http://192.0.2.1:9123".to_owned(),
        });
        assert_eq!(selected.id, "serial:one");
        assert_eq!(
            selected.fallback_address.as_deref(),
            Some("http://192.0.2.1:9123")
        );
    }
}
