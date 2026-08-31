//! Session D-Bus control surface for the desktop applet.
//!
//! The service runs on a dedicated thread with a blocking zbus connection,
//! mirroring the logind listener. It never writes to a Key Light directly:
//! property getters reflect state polled read-only from the devices, and every
//! control method forwards a [`ManualCommand`] to the single-writer controller.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, warn};
use zbus::blocking::connection;
use zbus::interface;

use crate::config::{LightConfig, SelectedLight};
use crate::domain::mired_to_kelvin;
use crate::light::resolve_selected;

const BUS_NAME: &str = "im.heiber.keylightd";
const ROOT_PATH: &str = "/im/heiber/keylightd";
const BASE_TICK: Duration = Duration::from_millis(250);
const LIGHT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A manual request that must be mediated by the single-writer controller.
pub enum ManualCommand {
    SetPower { light: usize, on: bool },
    TogglePower { light: usize },
    SetBrightness { light: usize, brightness: u8 },
    AdjustBrightness { light: usize, delta: i32 },
    SetTemperatureKelvin { light: usize, kelvin: u16 },
    AdjustTemperatureKelvin { light: usize, delta: i32 },
    SavePreset,
    ApplyPreset,
}

/// Cheap status published by the controller and reflected on the root object.
#[derive(Clone, Copy, Default)]
pub struct Status {
    pub camera_active: bool,
    pub has_preset: bool,
}

/// Shared status handle written by the controller and read by the service.
pub type SharedStatus = Arc<Mutex<Status>>;

fn send(commands: &Mutex<Sender<ManualCommand>>, command: ManualCommand) {
    if let Ok(sender) = commands.lock() {
        let _ = sender.send(command);
    }
}

struct Root {
    camera_active: bool,
    has_preset: bool,
    light_paths: Vec<zbus::zvariant::OwnedObjectPath>,
    commands: Mutex<Sender<ManualCommand>>,
}

#[interface(name = "im.heiber.keylightd1")]
impl Root {
    #[zbus(property)]
    fn camera_active(&self) -> bool {
        self.camera_active
    }

    #[zbus(property)]
    fn has_preset(&self) -> bool {
        self.has_preset
    }

    #[zbus(property)]
    fn light_paths(&self) -> Vec<zbus::zvariant::OwnedObjectPath> {
        self.light_paths.clone()
    }

    fn save_preset(&self) {
        send(&self.commands, ManualCommand::SavePreset);
    }

    fn apply_preset(&self) {
        send(&self.commands, ManualCommand::ApplyPreset);
    }
}

struct Light {
    index: usize,
    id: String,
    state: LightState,
    commands: Mutex<Sender<ManualCommand>>,
}

#[interface(name = "im.heiber.keylightd1.Light")]
impl Light {
    #[zbus(property)]
    fn id(&self) -> String {
        self.id.clone()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.state.name.clone()
    }

    #[zbus(property)]
    fn on(&self) -> bool {
        self.state.on
    }

    #[zbus(property)]
    fn brightness(&self) -> u8 {
        self.state.brightness
    }

    #[zbus(property)]
    fn temperature_kelvin(&self) -> u16 {
        self.state.temperature_kelvin
    }

    #[zbus(property)]
    fn reachable(&self) -> bool {
        self.state.reachable
    }

    fn set_power(&self, on: bool) {
        send(
            &self.commands,
            ManualCommand::SetPower {
                light: self.index,
                on,
            },
        );
    }

    fn toggle_power(&self) {
        send(
            &self.commands,
            ManualCommand::TogglePower { light: self.index },
        );
    }

    fn set_brightness(&self, brightness: u8) {
        send(
            &self.commands,
            ManualCommand::SetBrightness {
                light: self.index,
                brightness,
            },
        );
    }

    fn adjust_brightness(&self, delta: i32) {
        send(
            &self.commands,
            ManualCommand::AdjustBrightness {
                light: self.index,
                delta,
            },
        );
    }

    fn set_temperature_kelvin(&self, kelvin: u16) {
        send(
            &self.commands,
            ManualCommand::SetTemperatureKelvin {
                light: self.index,
                kelvin,
            },
        );
    }

    fn adjust_temperature_kelvin(&self, delta: i32) {
        send(
            &self.commands,
            ManualCommand::AdjustTemperatureKelvin {
                light: self.index,
                delta,
            },
        );
    }
}

/// Read-only view of a light's current state for the applet.
struct LightView {
    name: String,
    on: bool,
    brightness: u8,
    temperature_kelvin: u16,
    reachable: bool,
}

impl LightView {
    fn unreachable() -> Self {
        LightView {
            name: String::new(),
            on: false,
            brightness: 0,
            temperature_kelvin: 0,
            reachable: false,
        }
    }
}

fn poll_light(config: &LightConfig, selection: &SelectedLight) -> LightView {
    let resolved = resolve_selected(config, std::slice::from_ref(selection));
    let outcome = resolved
        .into_iter()
        .next()
        .and_then(|(_, light)| light.ok());
    let Some(light) = outcome else {
        return LightView::unreachable();
    };
    // accessory-info was fetched fresh while resolving, so this reflects the
    // current mobile-app display name, letting a rename surface on the next poll.
    let name = light.discovered().name.clone();
    match light.states() {
        Ok(states) => match states.first() {
            Some(state) => LightView {
                name,
                on: state.on,
                brightness: state.brightness,
                temperature_kelvin: mired_to_kelvin(state.temperature),
                reachable: true,
            },
            None => LightView::unreachable(),
        },
        Err(_) => LightView::unreachable(),
    }
}

/// The subset of a light's published properties that mirror the live device, and
/// which the applet observes through `PropertiesChanged`. Kept separate from the
/// D-Bus interface so the reconciliation policy is unit-testable without a bus.
#[derive(Default)]
struct LightState {
    name: String,
    on: bool,
    brightness: u8,
    temperature_kelvin: u16,
    reachable: bool,
}

/// Which properties changed during a `reconcile`, so the caller emits only the
/// corresponding `PropertiesChanged` signals.
#[derive(Default, Debug, PartialEq, Eq)]
struct LightChanges {
    name: bool,
    on: bool,
    brightness: bool,
    temperature_kelvin: bool,
    reachable: bool,
}

impl LightChanges {
    #[cfg(test)]
    fn any(&self) -> bool {
        self.name || self.on || self.brightness || self.temperature_kelvin || self.reachable
    }
}

impl LightState {
    /// Update toward the polled view, returning which properties changed. When
    /// the light is unreachable only reachability is updated; the last-known
    /// name, power, brightness, and temperature are preserved so the applet does
    /// not flicker to placeholder values while the device is briefly absent.
    fn reconcile(&mut self, view: &LightView) -> LightChanges {
        let mut changes = LightChanges::default();
        if self.reachable != view.reachable {
            self.reachable = view.reachable;
            changes.reachable = true;
        }
        // While unreachable the view carries no live values, so keep the last
        // known name, power, brightness, and temperature.
        if !view.reachable {
            return changes;
        }
        if self.name != view.name {
            self.name = view.name.clone();
            changes.name = true;
        }
        if self.on != view.on {
            self.on = view.on;
            changes.on = true;
        }
        if self.brightness != view.brightness {
            self.brightness = view.brightness;
            changes.brightness = true;
        }
        if self.temperature_kelvin != view.temperature_kelvin {
            self.temperature_kelvin = view.temperature_kelvin;
            changes.temperature_kelvin = true;
        }
        changes
    }
}

/// Serve the session D-Bus control surface until the process exits.
pub fn serve(
    selected: Vec<SelectedLight>,
    light_config: LightConfig,
    commands: Sender<ManualCommand>,
    status: SharedStatus,
) -> Result<()> {
    let paths: Vec<String> = (0..selected.len())
        .map(|index| format!("{ROOT_PATH}/light/{index}"))
        .collect();
    let light_paths = paths
        .iter()
        .map(|path| {
            zbus::zvariant::OwnedObjectPath::try_from(path.as_str())
                .context("build light object path")
        })
        .collect::<Result<Vec<_>>>()?;

    let mut builder = connection::Builder::session()
        .context("connect to session D-Bus")?
        .name(BUS_NAME)
        .context("request D-Bus name")?
        .serve_at(
            ROOT_PATH,
            Root {
                camera_active: false,
                has_preset: false,
                light_paths,
                commands: Mutex::new(commands.clone()),
            },
        )
        .context("register root interface")?;

    for (index, selection) in selected.iter().enumerate() {
        builder = builder
            .serve_at(
                paths[index].as_str(),
                Light {
                    index,
                    id: selection.id.clone(),
                    state: LightState {
                        name: selection.name.clone(),
                        ..Default::default()
                    },
                    commands: Mutex::new(commands.clone()),
                },
            )
            .context("register light interface")?;
    }

    let connection = builder.build().context("start session D-Bus service")?;
    let server = connection.object_server();
    let mut last_light_poll = Instant::now() - LIGHT_POLL_INTERVAL;

    loop {
        publish_root(&server, &status);
        if last_light_poll.elapsed() >= LIGHT_POLL_INTERVAL {
            for (index, selection) in selected.iter().enumerate() {
                let view = poll_light(&light_config, selection);
                publish_light(&server, paths[index].as_str(), &view);
            }
            last_light_poll = Instant::now();
        }
        thread::sleep(BASE_TICK);
    }
}

fn publish_root(server: &zbus::blocking::ObjectServer, status: &SharedStatus) {
    let snapshot = match status.lock() {
        Ok(status) => *status,
        Err(_) => return,
    };
    let root = match server.interface::<_, Root>(ROOT_PATH) {
        Ok(root) => root,
        Err(error) => {
            debug!(%error, "root interface unavailable");
            return;
        }
    };
    let mut iface = root.get_mut();
    if iface.camera_active != snapshot.camera_active {
        iface.camera_active = snapshot.camera_active;
        let _ = zbus::block_on(iface.camera_active_changed(root.signal_emitter()));
    }
    if iface.has_preset != snapshot.has_preset {
        iface.has_preset = snapshot.has_preset;
        let _ = zbus::block_on(iface.has_preset_changed(root.signal_emitter()));
    }
}

fn publish_light(server: &zbus::blocking::ObjectServer, path: &str, view: &LightView) {
    let light = match server.interface::<_, Light>(path) {
        Ok(light) => light,
        Err(error) => {
            debug!(%error, path, "light interface unavailable");
            return;
        }
    };
    let mut iface = light.get_mut();
    let changes = iface.state.reconcile(view);
    if changes.reachable {
        let _ = zbus::block_on(iface.reachable_changed(light.signal_emitter()));
    }
    if changes.name {
        let _ = zbus::block_on(iface.name_changed(light.signal_emitter()));
    }
    if changes.on {
        let _ = zbus::block_on(iface.on_changed(light.signal_emitter()));
    }
    if changes.brightness {
        let _ = zbus::block_on(iface.brightness_changed(light.signal_emitter()));
    }
    if changes.temperature_kelvin {
        let _ = zbus::block_on(iface.temperature_kelvin_changed(light.signal_emitter()));
    }
}

/// Spawn the control surface, logging and continuing if the session bus is
/// unavailable so the daemon still functions without a desktop session. Returns
/// the receiver the controller drains to apply manual commands.
pub fn spawn(
    selected: Vec<SelectedLight>,
    light_config: LightConfig,
    status: SharedStatus,
) -> std::sync::mpsc::Receiver<ManualCommand> {
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        if let Err(error) = serve(selected, light_config, sender, status) {
            warn!(%error, "session D-Bus control surface unavailable");
        }
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reachable_view(name: &str, on: bool, brightness: u8, kelvin: u16) -> LightView {
        LightView {
            name: name.to_owned(),
            on,
            brightness,
            temperature_kelvin: kelvin,
            reachable: true,
        }
    }

    #[test]
    fn reconcile_reports_every_initial_field_as_changed() {
        let mut state = LightState::default();
        let changes = state.reconcile(&reachable_view("Key", true, 60, 5000));
        assert_eq!(
            changes,
            LightChanges {
                name: true,
                on: true,
                brightness: true,
                temperature_kelvin: true,
                reachable: true,
            }
        );
        assert_eq!(state.name, "Key");
        assert!(state.on);
        assert_eq!(state.brightness, 60);
        assert_eq!(state.temperature_kelvin, 5000);
        assert!(state.reachable);
    }

    #[test]
    fn reconcile_reports_nothing_when_unchanged() {
        let mut state = LightState::default();
        state.reconcile(&reachable_view("Key", true, 60, 5000));
        let changes = state.reconcile(&reachable_view("Key", true, 60, 5000));
        assert!(!changes.any());
    }

    #[test]
    fn reconcile_surfaces_a_rename() {
        let mut state = LightState::default();
        state.reconcile(&reachable_view("Old", true, 60, 5000));
        let changes = state.reconcile(&reachable_view("New", true, 60, 5000));
        assert_eq!(
            changes,
            LightChanges {
                name: true,
                ..Default::default()
            }
        );
        assert_eq!(state.name, "New");
    }

    #[test]
    fn reconcile_preserves_last_known_values_when_unreachable() {
        let mut state = LightState::default();
        state.reconcile(&reachable_view("Key", true, 60, 5000));
        let changes = state.reconcile(&LightView::unreachable());
        assert_eq!(
            changes,
            LightChanges {
                reachable: true,
                ..Default::default()
            }
        );
        assert!(!state.reachable);
        assert_eq!(state.name, "Key");
        assert!(state.on);
        assert_eq!(state.brightness, 60);
        assert_eq!(state.temperature_kelvin, 5000);
    }

    #[test]
    fn reconcile_does_not_rename_to_empty_while_unreachable() {
        let mut state = LightState::default();
        state.reconcile(&reachable_view("Key", true, 60, 5000));
        state.reconcile(&LightView::unreachable());
        assert_eq!(state.name, "Key");
    }
}
