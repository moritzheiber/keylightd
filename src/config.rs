use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub light: LightConfig,
    pub camera: CameraConfig,
    pub ambient: AmbientConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LightConfig {
    pub address: Option<String>,
    pub discovery_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct CameraConfig {
    pub state_file: PathBuf,
    pub poll_interval_ms: u64,
    pub restore_grace_seconds: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AmbientConfig {
    pub sensor_path: Option<PathBuf>,
    pub calibration: Vec<CalibrationPoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CalibrationPoint {
    pub lux: f64,
    pub brightness: u8,
}

impl Default for LightConfig {
    fn default() -> Self {
        Self {
            address: None,
            discovery_timeout_seconds: 3,
        }
    }
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            state_file: PathBuf::from("/run/keylightd/camera-active"),
            poll_interval_ms: 250,
            restore_grace_seconds: 5,
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
        toml::from_str(&contents).with_context(|| format!("parse configuration {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create configuration directory {}", parent.display()))?;
        }
        let contents = toml::to_string_pretty(self).context("serialize configuration")?;
        fs::write(&path, contents)
            .with_context(|| format!("write configuration {}", path.display()))
    }
}

fn config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(Path::new(&path).join("keylightd/config.toml"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(Path::new(&home).join(".config/keylightd/config.toml"))
}
