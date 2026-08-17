use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use crate::ambient::{AmbientSensor, brightness_for_lux};
use crate::config::Config;
use crate::light::{KeyLight, LightState};

const INITIAL_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

enum Session {
    Idle,
    Starting {
        brightness: Option<u8>,
        retry_at: Instant,
        backoff: Duration,
    },
    Active {
        original: LightState,
    },
    Grace {
        original: LightState,
        deadline: Instant,
    },
    Restoring {
        original: LightState,
        retry_at: Instant,
        backoff: Duration,
    },
}

pub fn run() -> Result<()> {
    let config = Config::load()?;
    let running = Arc::new(AtomicBool::new(true));
    let signal_running = Arc::clone(&running);
    ctrlc::set_handler(move || signal_running.store(false, Ordering::Relaxed))
        .context("install signal handler")?;

    let poll_interval = Duration::from_millis(config.camera.poll_interval_ms);
    let restore_grace = Duration::from_secs(config.camera.restore_grace_seconds);
    let mut session = Session::Idle;

    while running.load(Ordering::Relaxed) {
        let camera_active = read_camera_state(&config);
        session = advance(session, camera_active, restore_grace, &config);
        thread::sleep(poll_interval);
    }
    info!("daemon stopped without restoring the Key Light");
    Ok(())
}

fn advance(
    session: Session,
    camera_active: bool,
    restore_grace: Duration,
    config: &Config,
) -> Session {
    match session {
        Session::Idle if camera_active => Session::Starting {
            brightness: call_brightness(config),
            retry_at: Instant::now(),
            backoff: INITIAL_RETRY,
        },
        Session::Starting { .. } if !camera_active => Session::Idle,
        Session::Starting {
            brightness,
            retry_at,
            backoff,
        } if Instant::now() >= retry_at => match apply_override(config, brightness) {
            Ok(original) => Session::Active { original },
            Err(error) => {
                warn!(%error, retry_seconds = backoff.as_secs(), "failed to activate Key Light");
                Session::Starting {
                    brightness,
                    retry_at: Instant::now() + backoff,
                    backoff: next_backoff(backoff),
                }
            }
        },
        Session::Active { original } if !camera_active => Session::Grace {
            original,
            deadline: Instant::now() + restore_grace,
        },
        Session::Grace { original, .. } if camera_active => Session::Active { original },
        Session::Grace { original, deadline } if Instant::now() >= deadline => {
            match restore(config, original) {
                Ok(()) => Session::Idle,
                Err(error) => {
                    warn!(%error, "failed to restore Key Light");
                    Session::Restoring {
                        original,
                        retry_at: Instant::now() + INITIAL_RETRY,
                        backoff: INITIAL_RETRY,
                    }
                }
            }
        }
        Session::Restoring { original, .. } if camera_active => Session::Active { original },
        Session::Restoring {
            original,
            retry_at,
            backoff,
        } if Instant::now() >= retry_at => match restore(config, original) {
            Ok(()) => Session::Idle,
            Err(error) => {
                warn!(%error, retry_seconds = backoff.as_secs(), "retrying Key Light restoration");
                Session::Restoring {
                    original,
                    retry_at: Instant::now() + backoff,
                    backoff: next_backoff(backoff),
                }
            }
        },
        current => current,
    }
}

fn read_camera_state(config: &Config) -> bool {
    match fs::read_to_string(&config.camera.state_file) {
        Ok(value) => value.trim() == "active",
        Err(error) => {
            debug!(
                %error,
                path = %config.camera.state_file.display(),
                "camera state unavailable"
            );
            false
        }
    }
}

fn call_brightness(config: &Config) -> Option<u8> {
    if config.ambient.calibration.is_empty() {
        info!("no ambient calibration; preserving current Key Light brightness");
        return None;
    }
    let sensor = match AmbientSensor::new(config.ambient.sensor_path.as_deref()) {
        Ok(sensor) => sensor,
        Err(error) => {
            warn!(%error, "ambient sensor unavailable; preserving current Key Light brightness");
            return None;
        }
    };
    match sensor.read_lux() {
        Ok(lux) => {
            let brightness = brightness_for_lux(&config.ambient.calibration, lux);
            info!(lux, brightness, "calculated call brightness");
            brightness
        }
        Err(error) => {
            error!(%error, "failed to read ambient sensor; preserving current brightness");
            None
        }
    }
}

fn apply_override(config: &Config, brightness: Option<u8>) -> Result<LightState> {
    let light = KeyLight::connect(&config.light)?;
    let original = light.state()?;
    let target = brightness.unwrap_or(original.brightness);
    light.set_power_brightness(true, target)?;
    info!(
        original_on = original.on,
        original_brightness = original.brightness,
        target,
        "activated Key Light for camera"
    );
    Ok(original)
}

fn restore(config: &Config, original: LightState) -> Result<()> {
    let light = KeyLight::connect(&config.light)?;
    light.set_power_brightness(original.on, original.brightness)?;
    info!(
        on = original.on,
        brightness = original.brightness,
        "restored Key Light state"
    );
    Ok(())
}

fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RETRY)
}
