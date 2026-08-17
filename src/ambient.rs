use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::CalibrationPoint;

pub struct AmbientSensor {
    path: PathBuf,
}

impl AmbientSensor {
    pub fn new(configured_path: Option<&Path>) -> Result<Self> {
        if let Some(path) = configured_path {
            if !path.is_file() {
                bail!("configured ambient sensor {} is not a file", path.display());
            }
            return Ok(Self {
                path: path.to_path_buf(),
            });
        }

        let root = Path::new("/sys/bus/iio/devices");
        let devices = fs::read_dir(root)
            .with_context(|| format!("ambient light sensor unavailable: {}", root.display()))?;
        for device in devices {
            let device = device.context("read IIO device entry")?.path();
            let input = device.join("in_illuminance_input");
            if input.is_file() {
                return Ok(Self { path: input });
            }
        }
        bail!("no IIO ambient light sensor exposes in_illuminance_input")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_lux(&self) -> Result<f64> {
        let value = fs::read_to_string(&self.path)
            .with_context(|| format!("read ambient sensor {}", self.path.display()))?;
        value
            .trim()
            .parse::<f64>()
            .with_context(|| format!("parse ambient sensor {}", self.path.display()))
    }
}

pub fn brightness_for_lux(points: &[CalibrationPoint], lux: f64) -> Option<u8> {
    let first = points.first()?;
    if lux <= first.lux {
        return Some(first.brightness);
    }
    let last = points.last()?;
    if lux >= last.lux {
        return Some(last.brightness);
    }

    points.windows(2).find_map(|window| {
        let low = &window[0];
        let high = &window[1];
        if !(low.lux..=high.lux).contains(&lux) {
            return None;
        }
        if high.lux <= low.lux {
            return Some(high.brightness);
        }
        let ratio = (lux - low.lux) / (high.lux - low.lux);
        let brightness = f64::from(low.brightness)
            + ratio * (f64::from(high.brightness) - f64::from(low.brightness));
        Some(brightness.round().clamp(1.0, 100.0) as u8)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(lux: f64, brightness: u8) -> CalibrationPoint {
        CalibrationPoint { lux, brightness }
    }

    #[test]
    fn interpolates_and_clamps_calibration() {
        let points = vec![point(10.0, 20), point(110.0, 80)];
        assert_eq!(brightness_for_lux(&points, 0.0), Some(20));
        assert_eq!(brightness_for_lux(&points, 60.0), Some(50));
        assert_eq!(brightness_for_lux(&points, 200.0), Some(80));
    }

    #[test]
    fn requires_calibration() {
        assert_eq!(brightness_for_lux(&[], 10.0), None);
    }
}
