use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::config::{CameraConfig, SelectedLight};

pub const STATE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CameraActivity {
    Active,
    Inactive,
    Stale,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CameraSnapshot {
    pub version: u32,
    pub heartbeat_ms: u64,
    pub cameras: Vec<CameraObservation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CameraObservation {
    pub id: String,
    pub name: String,
    pub devices: Vec<String>,
    pub last_frame_ms: Option<u64>,
}

impl CameraSnapshot {
    pub fn activity(&self, now_ms: u64, config: &CameraConfig) -> CameraActivity {
        if self.version != STATE_VERSION
            || now_ms.saturating_sub(self.heartbeat_ms) > config.helper_stale_seconds * 1_000
        {
            return CameraActivity::Stale;
        }
        if self.active_camera_ids(now_ms, config).is_empty() {
            CameraActivity::Inactive
        } else {
            CameraActivity::Active
        }
    }

    pub fn active_camera_ids(&self, now_ms: u64, config: &CameraConfig) -> HashSet<String> {
        if self.version != STATE_VERSION
            || now_ms.saturating_sub(self.heartbeat_ms) > config.helper_stale_seconds * 1_000
        {
            return HashSet::new();
        }
        let selected: Option<HashMap<&str, u64>> = config.selected.as_ref().map(|cameras| {
            cameras
                .iter()
                .map(|camera| {
                    let timeout = camera
                        .inactive_seconds
                        .unwrap_or(config.default_inactive_seconds)
                        .max(config.restore_grace_seconds);
                    (camera.id.as_str(), timeout)
                })
                .collect()
        });
        self.cameras
            .iter()
            .filter(|camera| {
                let timeout = match &selected {
                    Some(selected) => match selected.get(camera.id.as_str()) {
                        Some(timeout) => *timeout,
                        None => return false,
                    },
                    None => config
                        .default_inactive_seconds
                        .max(config.restore_grace_seconds),
                };
                camera
                    .last_frame_ms
                    .is_some_and(|last| now_ms.saturating_sub(last) <= timeout * 1_000)
            })
            .map(|camera| camera.id.clone())
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LogicalLightState {
    pub on: bool,
    pub brightness: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OwnedLight {
    pub selection: SelectedLight,
    pub original: Vec<LogicalLightState>,
    pub applied: Vec<LogicalLightState>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OwnershipJournal {
    pub version: u32,
    pub lights: BTreeMap<String, OwnedLight>,
}

impl Default for OwnershipJournal {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            lights: BTreeMap::new(),
        }
    }
}

pub fn override_states(
    original: &[LogicalLightState],
    brightness: Option<u8>,
) -> Vec<LogicalLightState> {
    original
        .iter()
        .map(|state| LogicalLightState {
            on: true,
            brightness: brightness.unwrap_or(state.brightness),
        })
        .collect()
}

pub fn ownership_matches(current: &[LogicalLightState], owned: &OwnedLight) -> bool {
    current == owned.applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CameraConfig, SelectedCamera};

    fn snapshot(heartbeat_ms: u64, frames: &[(&str, Option<u64>)]) -> CameraSnapshot {
        CameraSnapshot {
            version: STATE_VERSION,
            heartbeat_ms,
            cameras: frames
                .iter()
                .map(|(id, last_frame_ms)| CameraObservation {
                    id: (*id).to_owned(),
                    name: (*id).to_owned(),
                    devices: Vec::new(),
                    last_frame_ms: *last_frame_ms,
                })
                .collect(),
        }
    }

    #[test]
    fn aggregates_all_recent_cameras() {
        let config = CameraConfig::default();
        let active = snapshot(10_000, &[("one", Some(9_000)), ("two", Some(8_000))])
            .active_camera_ids(10_000, &config);
        assert_eq!(active, HashSet::from(["one".to_owned(), "two".to_owned()]));
    }

    #[test]
    fn stale_helper_state_is_inactive() {
        let config = CameraConfig::default();
        assert!(
            snapshot(1_000, &[("one", Some(1_000))])
                .active_camera_ids(10_000, &config)
                .is_empty()
        );
        assert_eq!(
            snapshot(1_000, &[("one", Some(1_000))]).activity(10_000, &config),
            CameraActivity::Stale
        );
    }

    #[test]
    fn explicit_selection_filters_and_uses_per_camera_timeout() {
        let config = CameraConfig {
            selected: Some(vec![SelectedCamera {
                id: "slow".to_owned(),
                name: "Slow".to_owned(),
                inactive_seconds: Some(10),
            }]),
            ..CameraConfig::default()
        };
        let active = snapshot(
            20_000,
            &[("slow", Some(11_000)), ("unselected", Some(20_000))],
        )
        .active_camera_ids(20_000, &config);
        assert_eq!(active, HashSet::from(["slow".to_owned()]));
    }

    #[test]
    fn overrides_every_logical_light_and_preserves_individual_brightness() {
        let original = vec![
            LogicalLightState {
                on: false,
                brightness: 20,
            },
            LogicalLightState {
                on: true,
                brightness: 70,
            },
        ];
        assert_eq!(
            override_states(&original, None),
            vec![
                LogicalLightState {
                    on: true,
                    brightness: 20,
                },
                LogicalLightState {
                    on: true,
                    brightness: 70,
                },
            ]
        );
        assert!(
            override_states(&original, Some(50))
                .iter()
                .all(|state| state.on && state.brightness == 50)
        );
    }

    #[test]
    fn inactivity_and_restoration_grace_overlap() {
        let mut config = CameraConfig {
            default_inactive_seconds: 2,
            restore_grace_seconds: 5,
            ..CameraConfig::default()
        };
        let state = snapshot(10_000, &[("one", Some(6_000))]);
        assert_eq!(state.activity(10_000, &config), CameraActivity::Active);
        assert_eq!(state.activity(11_001, &config), CameraActivity::Inactive);

        config.default_inactive_seconds = 8;
        assert_eq!(state.activity(13_000, &config), CameraActivity::Active);
    }
}
