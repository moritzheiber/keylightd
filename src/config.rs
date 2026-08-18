use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub light: LightConfig,
    pub camera: CameraConfig,
    pub ambient: AmbientConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LightConfig {
    pub selected: Option<Vec<SelectedLight>>,
    pub address: Option<String>,
    pub discovery_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedLight {
    pub id: String,
    pub name: String,
    pub service_name: Option<String>,
    pub fallback_address: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
    pub state_file: PathBuf,
    pub poll_interval_ms: u64,
    pub restore_grace_seconds: u64,
    pub helper_stale_seconds: u64,
    pub default_inactive_seconds: u64,
    pub selected: Option<Vec<SelectedCamera>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedCamera {
    pub id: String,
    pub name: String,
    pub inactive_seconds: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AmbientConfig {
    pub sensor_path: Option<PathBuf>,
    pub calibration: Vec<CalibrationPoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationPoint {
    pub lux: f64,
    pub brightness: u8,
}

impl Default for LightConfig {
    fn default() -> Self {
        Self {
            selected: None,
            address: None,
            discovery_timeout_seconds: 3,
        }
    }
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            state_file: PathBuf::from("/run/keylightd/camera-state.json"),
            poll_interval_ms: 250,
            restore_grace_seconds: 5,
            helper_stale_seconds: 3,
            default_inactive_seconds: 5,
            selected: None,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("parse configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create configuration directory {}", parent.display()))?;
        }
        let contents = toml::to_string_pretty(self).context("serialize configuration")?;
        let parent = path.parent().context("configuration path has no parent")?;
        let temporary = parent.join(".config.toml.tmp");
        fs::write(&temporary, contents)
            .with_context(|| format!("write configuration {}", temporary.display()))?;
        fs::rename(&temporary, &path)
            .with_context(|| format!("publish configuration {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.light.selected.as_ref().is_some_and(Vec::is_empty) {
            bail!("explicit light selection cannot be empty");
        }
        if self.camera.selected.as_ref().is_some_and(Vec::is_empty) {
            bail!("explicit camera selection cannot be empty");
        }
        if self.camera.poll_interval_ms == 0
            || self.camera.helper_stale_seconds == 0
            || self.camera.default_inactive_seconds == 0
        {
            bail!("camera timing values must be greater than zero");
        }
        validate_unique_ids(
            self.light
                .selected
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|light| light.id.as_str()),
            "light",
        )?;
        validate_unique_ids(
            self.camera
                .selected
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|camera| camera.id.as_str()),
            "camera",
        )?;
        for camera in self.camera.selected.as_deref().unwrap_or_default() {
            if camera.inactive_seconds == Some(0) {
                bail!("camera {} has a zero inactivity timeout", camera.id);
            }
        }
        let mut previous: Option<&CalibrationPoint> = None;
        for point in &self.ambient.calibration {
            if !point.lux.is_finite() || point.lux < 0.0 {
                bail!("calibration lux values must be finite and non-negative");
            }
            if !(1..=100).contains(&point.brightness) {
                bail!("calibration brightness must be between 1 and 100");
            }
            if let Some(previous) = previous {
                if point.lux <= previous.lux {
                    bail!("calibration lux values must be unique and increasing");
                }
                if point.brightness < previous.brightness {
                    bail!("calibration brightness must not decrease as lux increases");
                }
            }
            previous = Some(point);
        }
        Ok(())
    }
}

fn validate_unique_ids<'a>(ids: impl Iterator<Item = &'a str>, kind: &str) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if id.is_empty() {
            bail!("{kind} identity cannot be empty");
        }
        if !seen.insert(id) {
            bail!("duplicate {kind} identity {id}");
        }
    }
    Ok(())
}

fn config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(Path::new(&path).join("keylightd/config.toml"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(Path::new(&home).join(".config/keylightd/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_leave_device_selection_implicit() {
        let config = Config::default();
        assert!(config.light.selected.is_none());
        assert!(config.camera.selected.is_none());
        config.validate().unwrap();
    }

    #[test]
    fn explicit_empty_light_selection_is_invalid() {
        let mut config = Config::default();
        config.light.selected = Some(Vec::new());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("light selection")
        );
    }

    #[test]
    fn calibration_must_be_finite_unique_and_monotonic() {
        let mut config = Config::default();
        config.ambient.calibration = vec![
            CalibrationPoint {
                lux: 10.0,
                brightness: 50,
            },
            CalibrationPoint {
                lux: 10.0,
                brightness: 40,
            },
        ];
        assert!(config.validate().is_err());

        config.ambient.calibration[1].lux = 20.0;
        assert!(config.validate().is_err());

        config.ambient.calibration[1].brightness = 60;
        assert!(config.validate().is_ok());

        config.ambient.calibration[1].lux = f64::NAN;
        assert!(config.validate().is_err());
    }

    #[test]
    fn unknown_configuration_fields_are_rejected() {
        assert!(toml::from_str::<Config>("[camera]\nunknown = true\n").is_err());
    }
}
