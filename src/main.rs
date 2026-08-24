mod ambient;
mod camera;
mod config;
mod control;
mod daemon;
mod domain;
mod light;
mod time;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::ambient::AmbientSensor;
use crate::camera::CameraMonitor;
use crate::config::{CalibrationPoint, Config, PresetLightConfig, SelectedCamera, SelectedLight};
use crate::domain::CameraSnapshot;
use crate::light::{KeyLight, discover_all, resolve_selected, selected_from_discovered};

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
        #[arg(long, default_value = "/run/keylightd/camera-state.json")]
        state_file: PathBuf,
        #[arg(long, default_value = "/sys/kernel/tracing")]
        tracefs: PathBuf,
    },
    Lights {
        #[command(subcommand)]
        command: LightsCommand,
    },
    Cameras {
        #[command(subcommand)]
        command: CamerasCommand,
    },
    Discover,
    Sensor,
    Calibrate,
    Reload,
}

#[derive(Subcommand)]
enum LightsCommand {
    List,
    Select,
    /// Capture the current state of all selected lights as the preset.
    SavePreset,
    /// Apply the saved preset to all selected lights.
    ApplyPreset,
}

#[derive(Subcommand)]
enum CamerasCommand {
    List,
    Select,
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
        Command::Lights { command } => lights(command),
        Command::Cameras { command } => cameras(command),
        Command::Discover => discover(),
        Command::Sensor => sensor(),
        Command::Calibrate => calibrate(),
        Command::Reload => reload(),
    }
}

fn lights(command: LightsCommand) -> Result<()> {
    match command {
        LightsCommand::List => lights_list(),
        LightsCommand::Select => lights_select(),
        LightsCommand::SavePreset => lights_save_preset(),
        LightsCommand::ApplyPreset => lights_apply_preset(),
    }
}

fn lights_list() -> Result<()> {
    let config = Config::load()?;
    let discovered = discover_all(config.light.discovery_timeout_seconds)?;
    for (index, light) in discovered.iter().enumerate() {
        println!(
            "{}\t{}\t{}\t{}",
            index + 1,
            light.id,
            light.name,
            light.endpoint
        );
    }
    Ok(())
}

fn lights_select() -> Result<()> {
    let mut config = Config::load()?;
    let discovered = discover_all(config.light.discovery_timeout_seconds)?;
    if discovered.is_empty() {
        bail!("no Key Lights discovered; existing selection was not changed");
    }
    for (index, light) in discovered.iter().enumerate() {
        println!("{}. {} ({})", index + 1, light.name, light.id);
    }
    let selected = prompt_selection(discovered.len())?;
    if selected.is_empty() {
        println!("Selection unchanged");
        return Ok(());
    }
    config.light.selected = Some(
        selected
            .into_iter()
            .map(|index| selected_from_discovered(&discovered[index]))
            .collect(),
    );
    config.light.address = None;
    config.save()?;
    println!("Saved Key Light selection");
    Ok(())
}

fn lights_save_preset() -> Result<()> {
    let mut config = Config::load()?;
    let selected = ensure_light_selection(&mut config, true)?;
    let mut preset = Vec::new();
    for (selection, light) in resolve_selected(&config.light, &selected) {
        let light =
            light.with_context(|| format!("connect selected Key Light {}", selection.id))?;
        let state = light
            .states()?
            .into_iter()
            .next()
            .with_context(|| format!("Key Light {} has no logical lights", selection.id))?;
        let temperature_kelvin = crate::domain::mired_to_kelvin(state.temperature);
        preset.push(PresetLightConfig {
            id: selection.id.clone(),
            on: state.on,
            brightness: state.brightness,
            temperature_kelvin,
        });
        println!(
            "{}\ton={}\tbrightness={}\ttemperature={}K",
            selection.id, state.on, state.brightness, temperature_kelvin
        );
    }
    config.light.preset = Some(preset);
    config.save()?;
    println!("Saved preset");
    Ok(())
}

fn lights_apply_preset() -> Result<()> {
    let mut config = Config::load()?;
    if config.light.preset.is_none() {
        bail!("no preset saved; run `keylightd lights save-preset` first");
    }
    let selected = ensure_light_selection(&mut config, true)?;
    let mut failed = false;
    for (selection, light) in resolve_selected(&config.light, &selected) {
        let Some(target) = config.light.preset_for(&selection.id) else {
            eprintln!("{}\tSKIP\tno preset entry", selection.id);
            continue;
        };
        let result = light.and_then(|light| {
            let states = light
                .states()?
                .into_iter()
                .map(|_| crate::domain::LogicalLightState {
                    on: target.on,
                    brightness: target.brightness,
                    temperature: target.temperature,
                })
                .collect::<Vec<_>>();
            light.set_states(&states)
        });
        match result {
            Ok(()) => println!("{}\tapplied", selection.id),
            Err(error) => {
                failed = true;
                eprintln!("{}\tERROR\t{error:#}", selection.id);
            }
        }
    }
    if failed {
        bail!("failed to apply the preset to one or more lights");
    }
    println!("Applied preset");
    Ok(())
}

fn cameras(command: CamerasCommand) -> Result<()> {
    let mut config = Config::load()?;
    let snapshot = read_camera_snapshot(&config)?;
    match command {
        CamerasCommand::List => {
            for (index, camera) in snapshot.cameras.iter().enumerate() {
                println!(
                    "{}\t{}\t{}\t{}",
                    index + 1,
                    camera.id,
                    camera.name,
                    camera.devices.join(",")
                );
            }
            Ok(())
        }
        CamerasCommand::Select => {
            if snapshot.cameras.is_empty() {
                bail!("no cameras found; existing selection was not changed");
            }
            for (index, camera) in snapshot.cameras.iter().enumerate() {
                println!("{}. {} ({})", index + 1, camera.name, camera.id);
            }
            let selected = prompt_selection(snapshot.cameras.len())?;
            if selected.is_empty() {
                println!("Selection unchanged");
                return Ok(());
            }
            config.camera.selected = Some(
                selected
                    .into_iter()
                    .map(|index| {
                        let camera = &snapshot.cameras[index];
                        SelectedCamera {
                            id: camera.id.clone(),
                            name: camera.name.clone(),
                            inactive_seconds: None,
                        }
                    })
                    .collect(),
            );
            config.save()?;
            println!("Saved camera selection");
            Ok(())
        }
    }
}

fn discover() -> Result<()> {
    let mut config = Config::load()?;
    let selected = ensure_light_selection(&mut config, true)?;
    let mut failed = false;
    for (selection, light) in resolve_selected(&config.light, &selected) {
        match light.and_then(|light| {
            let states = light.states()?;
            println!(
                "{}\t{}\t{}\t{:?}",
                selection.id,
                selection.name,
                light.discovered().endpoint,
                states
            );
            Ok(())
        }) {
            Ok(()) => {}
            Err(error) => {
                failed = true;
                eprintln!("{}\tERROR\t{error:#}", selection.id);
            }
        }
    }
    if failed {
        bail!("one or more selected Key Lights were unreachable");
    }
    Ok(())
}

fn sensor() -> Result<()> {
    let config = Config::load()?;
    let sensor = AmbientSensor::new(config.ambient.sensor_path.as_deref())?;
    println!(
        "{:.2} lux ({})",
        sensor.read_lux()?,
        sensor.path().display()
    );
    Ok(())
}

fn calibrate() -> Result<()> {
    let mut config = Config::load()?;
    let sensor = AmbientSensor::new(config.ambient.sensor_path.as_deref())?;
    let lux = sensor.read_lux()?;
    if !lux.is_finite() || lux < 0.0 {
        bail!("ambient sensor returned invalid lux value {lux}");
    }
    let selected = ensure_light_selection(&mut config, true)?;
    let mut lights: Vec<KeyLight> = resolve_selected(&config.light, &selected)
        .into_iter()
        .map(|(selection, light)| {
            light.with_context(|| format!("connect selected Key Light {}", selection.id))
        })
        .collect::<Result<_>>()?;
    let first = lights.first().context("no selected Key Lights")?;
    let original = first.states()?;
    let default_brightness = original
        .first()
        .context("selected Key Light has no logical lights")?
        .brightness;

    println!("Ambient light: {lux:.2} lux");
    print!("Desired brightness (1-100) [{default_brightness}]: ");
    io::stdout().flush().context("flush calibration prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("read calibration brightness")?;
    let brightness = if input.trim().is_empty() {
        default_brightness
    } else {
        input
            .trim()
            .parse::<u8>()
            .context("brightness must be an integer")?
    };
    if !(1..=100).contains(&brightness) {
        bail!("brightness must be between 1 and 100");
    }
    let mut updated_config = config.clone();
    updated_config
        .ambient
        .calibration
        .retain(|point| point.lux.total_cmp(&lux).is_ne());
    updated_config
        .ambient
        .calibration
        .push(CalibrationPoint { lux, brightness });
    updated_config
        .ambient
        .calibration
        .sort_by(|left, right| left.lux.total_cmp(&right.lux));
    updated_config.validate()?;
    for light in &mut lights {
        let states = light
            .states()?
            .into_iter()
            .map(|state| crate::domain::LogicalLightState {
                on: true,
                brightness,
                temperature: state.temperature,
            })
            .collect::<Vec<_>>();
        light.set_states(&states)?;
    }
    updated_config.save()?;
    info!(lux, brightness, "saved calibration point");
    println!("Saved {lux:.2} lux -> {brightness}%");
    Ok(())
}

fn reload() -> Result<()> {
    let status = ProcessCommand::new("systemctl")
        .args(["--user", "kill", "--signal=HUP", "keylightd.service"])
        .status()
        .context("run systemctl --user kill")?;
    if !status.success() {
        bail!("failed to signal keylightd.service");
    }
    Ok(())
}

pub(crate) fn ensure_light_selection(
    config: &mut Config,
    persist_auto: bool,
) -> Result<Vec<SelectedLight>> {
    if let Some(selected) = &config.light.selected {
        return Ok(selected.clone());
    }
    let discovered = discover_all(config.light.discovery_timeout_seconds)?;
    match discovered.as_slice() {
        [] => bail!("no Key Lights discovered; run `keylightd lights select` when available"),
        [light] => {
            let mut selected = selected_from_discovered(light);
            if let Some(address) = config.light.address.take() {
                selected.fallback_address = Some(address);
            }
            config.light.selected = Some(vec![selected.clone()]);
            if persist_auto {
                config.save()?;
            }
            Ok(vec![selected])
        }
        _ => bail!(
            "multiple Key Lights discovered; run `keylightd lights select` to choose one or more"
        ),
    }
}

fn read_camera_snapshot(config: &Config) -> Result<CameraSnapshot> {
    let contents = fs::read_to_string(&config.camera.state_file)
        .with_context(|| format!("read {}", config.camera.state_file.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parse {}", config.camera.state_file.display()))
}

fn prompt_selection(count: usize) -> Result<Vec<usize>> {
    print!("Select one or more numbers separated by commas (blank keeps current): ");
    io::stdout().flush().context("flush selection prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("read selection")?;
    parse_selection(&input, count)
}

fn parse_selection(input: &str, count: usize) -> Result<Vec<usize>> {
    if input.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut selected = Vec::new();
    for value in input.split(',') {
        let number = value
            .trim()
            .parse::<usize>()
            .with_context(|| format!("invalid selection {}", value.trim()))?;
        if number == 0 || number > count {
            bail!("selection {number} is outside 1-{count}");
        }
        let index = number - 1;
        if !selected.contains(&index) {
            selected.push(index);
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_unique_selections() {
        assert_eq!(parse_selection("2, 1, 2", 3).unwrap(), vec![1, 0]);
        assert!(parse_selection("4", 3).is_err());
        assert!(parse_selection("", 3).unwrap().is_empty());
    }
}
