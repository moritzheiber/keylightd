mod ambient;
mod camera;
mod config;
mod daemon;
mod light;

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::ambient::AmbientSensor;
use crate::camera::CameraMonitor;
use crate::config::{CalibrationPoint, Config};
use crate::light::KeyLight;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Daemon,
    CameraMonitor {
        #[arg(long, default_value = "/run/keylightd/camera-active")]
        state_file: PathBuf,
        #[arg(long, default_value = "/sys/kernel/tracing")]
        tracefs: PathBuf,
    },
    Discover,
    Sensor,
    Calibrate,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("keylightd=info")),
        )
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Daemon => daemon::run(),
        Command::CameraMonitor {
            state_file,
            tracefs,
        } => CameraMonitor::new(tracefs, state_file).run(),
        Command::Discover => {
            let config = Config::load()?;
            let light = KeyLight::connect(&config.light)?;
            let state = light.state()?;
            println!(
                "{} (power: {}, brightness: {}%)",
                light.endpoint(),
                if state.on { "on" } else { "off" },
                state.brightness
            );
            Ok(())
        }
        Command::Sensor => {
            let config = Config::load()?;
            let sensor = AmbientSensor::new(config.ambient.sensor_path.as_deref())?;
            println!(
                "{:.2} lux ({})",
                sensor.read_lux()?,
                sensor.path().display()
            );
            Ok(())
        }
        Command::Calibrate => calibrate(),
    }
}

fn calibrate() -> Result<()> {
    let mut config = Config::load()?;
    let sensor = AmbientSensor::new(config.ambient.sensor_path.as_deref())?;
    let lux = sensor.read_lux()?;
    let light = KeyLight::connect(&config.light)?;
    let original = light.state()?;

    println!("Ambient light: {lux:.2} lux");
    print!(
        "Desired Key Light brightness (1-100) [{}]: ",
        original.brightness
    );
    io::stdout().flush().context("flush calibration prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("read calibration brightness")?;
    let brightness = if input.trim().is_empty() {
        original.brightness
    } else {
        input
            .trim()
            .parse::<u8>()
            .context("brightness must be an integer")?
    };
    if !(1..=100).contains(&brightness) {
        bail!("brightness must be between 1 and 100");
    }

    light.set_power_brightness(true, brightness)?;
    config
        .ambient
        .calibration
        .retain(|point| (point.lux - lux).abs() >= 0.01);
    config
        .ambient
        .calibration
        .push(CalibrationPoint { lux, brightness });
    config
        .ambient
        .calibration
        .sort_by(|left, right| left.lux.total_cmp(&right.lux));
    config.save()?;
    info!(lux, brightness, "saved calibration point");
    println!("Saved {lux:.2} lux -> {brightness}%");
    Ok(())
}
