use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::config::{CameraConfig, SelectedLight};

pub const STATE_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CameraActivity {
    Active,
    Inactive,
    Stale,
}

/// Colour-temperature bounds. The Elgato devices speak mired; users speak Kelvin.
pub const MIRED_MIN: u16 = 143; // 7000 K, coolest
pub const MIRED_MAX: u16 = 344; // 2900 K, warmest
pub const KELVIN_MIN: u16 = 2900;
pub const KELVIN_MAX: u16 = 7000;

/// Convert a Kelvin value to the device's mired units, clamped to the device range.
pub fn kelvin_to_mired(kelvin: u16) -> u16 {
    let kelvin = kelvin.clamp(KELVIN_MIN, KELVIN_MAX);
    let mired = (1_000_000.0 / f64::from(kelvin)).round() as u16;
    mired.clamp(MIRED_MIN, MIRED_MAX)
}

/// Convert a device mired value to Kelvin, clamped to the exposed range.
pub fn mired_to_kelvin(mired: u16) -> u16 {
    let mired = mired.clamp(MIRED_MIN, MIRED_MAX);
    let kelvin = (1_000_000.0 / f64::from(mired)).round() as u16;
    kelvin.clamp(KELVIN_MIN, KELVIN_MAX)
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
    /// Colour temperature in the device's mired units.
    pub temperature: u16,
}

/// A saved or target look for one physical light, applied uniformly to every
/// logical light of that device. Temperature is stored in device mired units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresetLight {
    pub on: bool,
    pub brightness: u8,
    pub temperature: u16,
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

/// Compute the applied state for every logical light of one device when a camera
/// session begins, following the precedence of calibrated brightness, then the
/// saved preset, then preserving the current state.
///
/// Calibration sets brightness only and leaves colour temperature unchanged.
/// A preset is applied faithfully, so a light saved off is set off. Preserving
/// keeps brightness and temperature and only powers the light on.
pub fn session_target_states(
    original: &[LogicalLightState],
    calibrated_brightness: Option<u8>,
    preset: Option<PresetLight>,
) -> Vec<LogicalLightState> {
    original
        .iter()
        .map(|state| {
            if let Some(brightness) = calibrated_brightness {
                LogicalLightState {
                    on: true,
                    brightness,
                    temperature: state.temperature,
                }
            } else if let Some(preset) = preset {
                LogicalLightState {
                    on: preset.on,
                    brightness: preset.brightness,
                    temperature: preset.temperature,
                }
            } else {
                LogicalLightState {
                    on: true,
                    brightness: state.brightness,
                    temperature: state.temperature,
                }
            }
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
    fn calibration_sets_brightness_and_preserves_temperature() {
        let original = vec![
            LogicalLightState {
                on: false,
                brightness: 20,
                temperature: 200,
            },
            LogicalLightState {
                on: true,
                brightness: 70,
                temperature: 250,
            },
        ];
        assert_eq!(
            session_target_states(&original, Some(50), None),
            vec![
                LogicalLightState {
                    on: true,
                    brightness: 50,
                    temperature: 200,
                },
                LogicalLightState {
                    on: true,
                    brightness: 50,
                    temperature: 250,
                },
            ]
        );
    }

    #[test]
    fn preserve_powers_on_and_keeps_brightness_and_temperature() {
        let original = vec![LogicalLightState {
            on: false,
            brightness: 33,
            temperature: 210,
        }];
        assert_eq!(
            session_target_states(&original, None, None),
            vec![LogicalLightState {
                on: true,
                brightness: 33,
                temperature: 210,
            }]
        );
    }

    #[test]
    fn preset_is_applied_faithfully_including_saved_off() {
        let original = vec![LogicalLightState {
            on: true,
            brightness: 90,
            temperature: 150,
        }];
        let off_preset = PresetLight {
            on: false,
            brightness: 60,
            temperature: 222,
        };
        assert_eq!(
            session_target_states(&original, None, Some(off_preset)),
            vec![LogicalLightState {
                on: false,
                brightness: 60,
                temperature: 222,
            }]
        );
    }

    #[test]
    fn calibration_takes_precedence_over_preset() {
        let original = vec![LogicalLightState {
            on: true,
            brightness: 40,
            temperature: 300,
        }];
        let preset = PresetLight {
            on: false,
            brightness: 10,
            temperature: 143,
        };
        assert_eq!(
            session_target_states(&original, Some(80), Some(preset)),
            vec![LogicalLightState {
                on: true,
                brightness: 80,
                temperature: 300,
            }]
        );
    }

    #[test]
    fn device_mired_values_round_trip_through_kelvin() {
        for mired in MIRED_MIN..=MIRED_MAX {
            assert_eq!(kelvin_to_mired(mired_to_kelvin(mired)), mired);
        }
    }

    #[test]
    fn kelvin_and_mired_conversions_clamp_to_range() {
        assert_eq!(kelvin_to_mired(1_000), MIRED_MAX);
        assert_eq!(kelvin_to_mired(60_000), MIRED_MIN);
        assert_eq!(mired_to_kelvin(0), mired_to_kelvin(MIRED_MIN));
        assert_eq!(mired_to_kelvin(10_000), mired_to_kelvin(MIRED_MAX));
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
