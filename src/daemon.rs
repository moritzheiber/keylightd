use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use tracing::{debug, error, info, warn};

use crate::ambient::{AmbientSensor, brightness_for_lux};
use crate::config::{Config, PresetLightConfig, SelectedLight};
use crate::control::{self, ManualCommand, SharedStatus, Status};
use crate::domain::{
    CameraActivity, CameraSnapshot, KELVIN_MAX, KELVIN_MIN, LogicalLightState, OwnedLight,
    OwnershipJournal, STATE_VERSION, kelvin_to_mired, mired_to_kelvin, ownership_matches,
    session_target_states,
};
use crate::ensure_light_selection;
use crate::light::{KeyLight, resolve_selected};
use crate::time::boottime_ms;

const INITIAL_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);
const LOCK_NAME: &[u8] = b"keylightd-user-daemon";

enum Event {
    Reload,
    Shutdown,
    Suspend {
        start: bool,
        ready: Option<mpsc::SyncSender<()>>,
    },
}

struct Retry {
    at: Instant,
    backoff: Duration,
}

impl Retry {
    fn immediate() -> Self {
        Self {
            at: Instant::now(),
            backoff: INITIAL_RETRY,
        }
    }

    fn failed(self) -> Self {
        Self {
            at: Instant::now() + self.backoff,
            backoff: self.backoff.saturating_mul(2).min(MAX_RETRY),
        }
    }
}

struct Controller {
    config: Config,
    selected: Vec<SelectedLight>,
    journal: OwnershipJournal,
    unreconciled: HashSet<String>,
    retries: BTreeMap<String, Retry>,
    camera_active: bool,
    camera_initialized: bool,
    grace_deadline: Option<Instant>,
    restoring: bool,
    suspended: bool,
    shutting_down: bool,
    session_brightness: Option<u8>,
    suspend_ready: Option<mpsc::SyncSender<()>>,
    status: SharedStatus,
}

pub fn run() -> Result<()> {
    // A shutdown flag lets the lock wait below respond to systemd stop/restart
    // promptly. signal_hook chains this handler with the main-loop Signals
    // handler installed later in event_channel(), so both observe every signal.
    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))
            .context("register shutdown flag for the ownership wait")?;
    }

    let _lock = match wait_for_lock(&shutdown) {
        LockOutcome::Acquired(lock) => lock,
        LockOutcome::Shutdown => return Ok(()),
        LockOutcome::Failed(error) => {
            return Err(error).context("acquire machine-wide daemon lock");
        }
    };
    let journal = load_journal()?;
    let mut config = Config::load()?;
    let selected = if config.light.selected.is_none() && !journal.lights.is_empty() {
        let selected = journal
            .lights
            .values()
            .map(|owned| owned.selection.clone())
            .collect::<Vec<_>>();
        config.light.selected = Some(selected.clone());
        config.save()?;
        selected
    } else {
        ensure_light_selection(&mut config, true)?
    };
    let unreconciled = journal.lights.keys().cloned().collect();
    let mut controller = Controller {
        config,
        selected,
        journal,
        unreconciled,
        retries: BTreeMap::new(),
        camera_active: false,
        camera_initialized: false,
        grace_deadline: None,
        restoring: false,
        suspended: false,
        shutting_down: false,
        session_brightness: None,
        suspend_ready: None,
        status: Arc::new(Mutex::new(Status::default())),
    };
    controller.retries.extend(
        controller
            .journal
            .lights
            .keys()
            .cloned()
            .map(|id| (id, Retry::immediate())),
    );
    controller.publish_status();
    let manual = control::spawn(
        controller.selected.clone(),
        controller.config.light.clone(),
        Arc::clone(&controller.status),
    );
    let events = event_channel()?;

    // A signal delivered between flag registration and the Signals handler being
    // installed above sets the flag but is not seen by the event channel; honour
    // it here so a stop during startup still restores and exits cleanly.
    if shutdown.load(Ordering::Relaxed) {
        controller.handle_event(Event::Shutdown);
    }

    loop {
        while let Ok(event) = events.try_recv() {
            controller.handle_event(event);
        }
        while let Ok(command) = manual.try_recv() {
            controller.handle_manual(command);
        }
        let activity = if controller.suspended || controller.shutting_down {
            CameraActivity::Inactive
        } else {
            read_camera_activity(&controller.config)
        };
        controller.tick(activity)?;
        controller.publish_status();
        if controller.suspended
            && controller.journal.lights.is_empty()
            && let Some(ready) = controller.suspend_ready.take()
        {
            let _ = ready.send(());
        }
        if controller.shutting_down && controller.journal.lights.is_empty() {
            info!("restored all owned Key Lights before shutdown");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(
            controller.config.camera.poll_interval_ms,
        ));
    }
}

impl Controller {
    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Reload => match Config::load().and_then(|mut config| {
                let selected = ensure_light_selection(&mut config, false)?;
                Ok((config, selected))
            }) {
                Ok((config, selected)) => {
                    self.config = config;
                    self.selected = selected;
                    info!("reloaded configuration");
                }
                Err(error) => {
                    error!(%error, "configuration reload failed; retaining last valid configuration");
                }
            },
            Event::Shutdown => {
                self.shutting_down = true;
                self.begin_restoration();
                info!("graceful shutdown requested");
            }
            Event::Suspend { start: true, ready } => {
                self.suspended = true;
                self.suspend_ready = ready;
                self.begin_restoration();
                info!("restoring lights before suspend");
            }
            Event::Suspend { start: false, .. } => {
                self.suspended = false;
                self.suspend_ready = None;
                self.camera_active = false;
                self.camera_initialized = false;
                self.session_brightness = None;
                info!("resumed; awaiting a new camera session");
            }
        }
    }

    fn publish_status(&self) {
        if let Ok(mut status) = self.status.lock() {
            status.camera_active = self.camera_active;
            status.has_preset = self.config.light.preset.is_some();
        }
    }

    fn handle_manual(&mut self, command: ManualCommand) {
        let result = match command {
            ManualCommand::SetPower { light, on } => self.manual_light(light, |states| {
                for state in states.iter_mut() {
                    state.on = on;
                }
            }),
            ManualCommand::TogglePower { light } => self.manual_light(light, |states| {
                let any_on = states.iter().any(|state| state.on);
                for state in states.iter_mut() {
                    state.on = !any_on;
                }
            }),
            ManualCommand::SetBrightness { light, brightness } => {
                let brightness = brightness.clamp(1, 100);
                self.manual_light(light, |states| {
                    for state in states.iter_mut() {
                        state.brightness = brightness;
                    }
                })
            }
            ManualCommand::AdjustBrightness { light, delta } => {
                self.manual_light(light, |states| {
                    for state in states.iter_mut() {
                        state.brightness = adjust(i32::from(state.brightness), delta, 1, 100) as u8;
                    }
                })
            }
            ManualCommand::SetTemperatureKelvin { light, kelvin } => {
                let mired = kelvin_to_mired(kelvin);
                self.manual_light(light, |states| {
                    for state in states.iter_mut() {
                        state.temperature = mired;
                    }
                })
            }
            ManualCommand::AdjustTemperatureKelvin { light, delta } => {
                self.manual_light(light, |states| {
                    for state in states.iter_mut() {
                        let kelvin = adjust(
                            i32::from(mired_to_kelvin(state.temperature)),
                            delta,
                            i32::from(KELVIN_MIN),
                            i32::from(KELVIN_MAX),
                        ) as u16;
                        state.temperature = kelvin_to_mired(kelvin);
                    }
                })
            }
            ManualCommand::SavePreset => self.manual_save_preset(),
            ManualCommand::ApplyPreset => self.manual_apply_preset(),
        };
        if let Err(error) = result {
            warn!(%error, "manual command failed");
        }
    }

    fn manual_light(
        &mut self,
        index: usize,
        transform: impl Fn(&mut Vec<LogicalLightState>),
    ) -> Result<()> {
        let Some(selection) = self.selected.get(index).cloned() else {
            return Ok(());
        };
        for (id, light) in resolve_selected(&self.config.light, std::slice::from_ref(&selection)) {
            let light = light?;
            let mut states = light.states()?;
            transform(&mut states);
            light.set_states(&states)?;
            if let Some(owned) = self.journal.lights.get_mut(&id.id) {
                owned.applied = states;
                save_journal(&self.journal)?;
            }
        }
        Ok(())
    }

    fn manual_save_preset(&mut self) -> Result<()> {
        let mut preset = Vec::new();
        for selection in self.selected.clone() {
            for (id, light) in
                resolve_selected(&self.config.light, std::slice::from_ref(&selection))
            {
                let light = light?;
                if let Some(state) = light.states()?.into_iter().next() {
                    preset.push(PresetLightConfig {
                        id: id.id.clone(),
                        on: state.on,
                        brightness: state.brightness,
                        temperature_kelvin: mired_to_kelvin(state.temperature),
                    });
                }
            }
        }
        self.config.light.preset = Some(preset);
        self.config.save()?;
        self.publish_status();
        info!("saved preset from applet");
        Ok(())
    }

    fn manual_apply_preset(&mut self) -> Result<()> {
        if self.config.light.preset.is_none() {
            return Ok(());
        }
        for selection in self.selected.clone() {
            let Some(target) = self.config.light.preset_for(&selection.id) else {
                continue;
            };
            for (id, light) in
                resolve_selected(&self.config.light, std::slice::from_ref(&selection))
            {
                let light = light?;
                let mut states = light.states()?;
                for state in states.iter_mut() {
                    state.on = target.on;
                    state.brightness = target.brightness;
                    state.temperature = target.temperature;
                }
                light.set_states(&states)?;
                if let Some(owned) = self.journal.lights.get_mut(&id.id) {
                    owned.applied = states;
                    save_journal(&self.journal)?;
                }
            }
        }
        info!("applied preset from applet");
        Ok(())
    }

    fn tick(&mut self, activity: CameraActivity) -> Result<()> {
        if activity == CameraActivity::Active {
            if !self.camera_active {
                self.session_brightness = call_brightness(&self.config);
                self.retries.extend(
                    self.selected
                        .iter()
                        .filter(|selection| !self.journal.lights.contains_key(&selection.id))
                        .map(|selection| (selection.id.clone(), Retry::immediate())),
                );
                self.retries.extend(
                    self.journal
                        .lights
                        .keys()
                        .cloned()
                        .map(|id| (id, Retry::immediate())),
                );
            }
            self.camera_initialized = true;
            self.camera_active = true;
            self.grace_deadline = None;
            self.restoring = false;
            self.activate_selected()?;
            return Ok(());
        }

        if activity == CameraActivity::Stale {
            if self.camera_active || (!self.camera_initialized && !self.journal.lights.is_empty()) {
                self.camera_active = false;
                self.camera_initialized = true;
                self.grace_deadline = Some(
                    Instant::now() + Duration::from_secs(self.config.camera.restore_grace_seconds),
                );
                info!("camera helper stale; restoration grace started");
            }
        } else if self.camera_active
            || (!self.camera_initialized && !self.journal.lights.is_empty())
        {
            self.camera_active = false;
            self.camera_initialized = true;
            self.begin_restoration();
            info!("camera activity stopped; restoration started");
        } else {
            self.camera_initialized = true;
        }
        if self
            .grace_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.begin_restoration();
        }
        if self.restoring {
            self.restore_owned()?;
        }
        Ok(())
    }

    fn activate_selected(&mut self) -> Result<()> {
        let selected_ids: HashSet<&str> = self
            .selected
            .iter()
            .map(|light| light.id.as_str())
            .collect();
        for selection in &self.selected {
            if !self.journal.lights.contains_key(&selection.id) {
                self.retries
                    .entry(selection.id.clone())
                    .or_insert_with(Retry::immediate);
            }
        }
        let due = self
            .retries
            .iter()
            .filter(|(id, retry)| selected_ids.contains(id.as_str()) && Instant::now() >= retry.at)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if due.is_empty() {
            return Ok(());
        }
        let selections = self
            .selected
            .iter()
            .filter(|selection| due.contains(&selection.id))
            .cloned()
            .collect::<Vec<_>>();
        for (selection, light) in resolve_selected(&self.config.light, &selections) {
            let result = light.and_then(|light| self.activate_light(&selection, &light));
            self.finish_attempt(&selection.id, result);
        }
        Ok(())
    }

    fn activate_light(&mut self, selection: &SelectedLight, light: &KeyLight) -> Result<()> {
        if self.unreconciled.remove(&selection.id)
            && let Some(owned) = self.journal.lights.get(&selection.id)
        {
            let current = light.states()?;
            if !ownership_matches(&current, owned) {
                warn!(
                    light_id = selection.id,
                    "abandoning stale ownership after external state change"
                );
                self.journal.lights.remove(&selection.id);
                save_journal(&self.journal)?;
            }
        }
        if let Some(owned) = self.journal.lights.get(&selection.id) {
            light.set_states(&owned.applied)?;
            return Ok(());
        }

        let original = light.states()?;
        let preset = self.config.light.preset_for(&selection.id);
        let applied = session_target_states(&original, self.session_brightness, preset);
        self.journal.lights.insert(
            selection.id.clone(),
            OwnedLight {
                selection: selection.clone(),
                original,
                applied: applied.clone(),
            },
        );
        save_journal(&self.journal)?;
        light.set_states(&applied)?;
        Ok(())
    }

    fn begin_restoration(&mut self) {
        self.grace_deadline = None;
        self.restoring = true;
        self.retries
            .retain(|id, _| self.journal.lights.contains_key(id));
        self.retries.extend(
            self.journal
                .lights
                .keys()
                .cloned()
                .map(|id| (id, Retry::immediate())),
        );
    }

    fn restore_owned(&mut self) -> Result<()> {
        let due = self
            .journal
            .lights
            .values()
            .filter(|owned| {
                self.retries
                    .get(&owned.selection.id)
                    .is_none_or(|retry| Instant::now() >= retry.at)
            })
            .map(|owned| owned.selection.clone())
            .collect::<Vec<_>>();
        if due.is_empty() {
            if self.journal.lights.is_empty() {
                self.restoring = false;
                self.session_brightness = None;
            }
            return Ok(());
        }
        for (selection, light) in resolve_selected(&self.config.light, &due) {
            let result = light.and_then(|light| self.restore_light(&selection, &light));
            self.finish_attempt(&selection.id, result);
        }
        if self.journal.lights.is_empty() {
            self.restoring = false;
            self.session_brightness = None;
        }
        Ok(())
    }

    fn restore_light(&mut self, selection: &SelectedLight, light: &KeyLight) -> Result<()> {
        if self.unreconciled.remove(&selection.id)
            && let Some(owned) = self.journal.lights.get(&selection.id)
        {
            let current = light.states()?;
            if !ownership_matches(&current, owned) {
                warn!(
                    light_id = selection.id,
                    "abandoning stale ownership after external state change"
                );
                self.journal.lights.remove(&selection.id);
                save_journal(&self.journal)?;
                return Ok(());
            }
        }
        let original = self
            .journal
            .lights
            .get(&selection.id)
            .context("owned light disappeared from journal")?
            .original
            .clone();
        light.set_states(&original)?;
        self.journal.lights.remove(&selection.id);
        save_journal(&self.journal)
    }

    fn finish_attempt(&mut self, id: &str, result: Result<()>) {
        match result {
            Ok(()) => {
                self.retries.remove(id);
            }
            Err(error) => {
                let retry = self.retries.remove(id).unwrap_or_else(Retry::immediate);
                let retry = retry.failed();
                warn!(
                    %error,
                    light_id = id,
                    retry_seconds = retry.backoff.as_secs(),
                    "Key Light operation failed"
                );
                self.retries.insert(id.to_owned(), retry);
            }
        }
    }
}

fn read_camera_activity(config: &Config) -> CameraActivity {
    match fs::read_to_string(&config.camera.state_file) {
        Ok(contents) => match serde_json::from_str::<CameraSnapshot>(&contents) {
            Ok(snapshot) => snapshot.activity(boottime_ms().unwrap_or_default(), &config.camera),
            Err(error) => {
                warn!(
                    %error,
                    path = %config.camera.state_file.display(),
                    "camera snapshot is invalid"
                );
                CameraActivity::Stale
            }
        },
        Err(error) => {
            debug!(
                %error,
                path = %config.camera.state_file.display(),
                "camera snapshot unavailable"
            );
            CameraActivity::Stale
        }
    }
}

fn call_brightness(config: &Config) -> Option<u8> {
    if config.ambient.calibration.is_empty() {
        info!("no ambient calibration; preserving current brightness");
        return None;
    }
    let sensor = match AmbientSensor::new(config.ambient.sensor_path.as_deref()) {
        Ok(sensor) => sensor,
        Err(error) => {
            warn!(%error, "ambient sensor unavailable; preserving brightness");
            return None;
        }
    };
    match sensor.read_lux() {
        Ok(lux) if lux.is_finite() && lux >= 0.0 => {
            brightness_for_lux(&config.ambient.calibration, lux)
        }
        Ok(lux) => {
            warn!(
                lux,
                "ambient sensor returned invalid value; preserving brightness"
            );
            None
        }
        Err(error) => {
            warn!(%error, "ambient sensor unavailable; preserving brightness");
            None
        }
    }
}

fn adjust(current: i32, delta: i32, min: i32, max: i32) -> i32 {
    current.saturating_add(delta).clamp(min, max)
}

fn event_channel() -> Result<Receiver<Event>> {
    let (sender, receiver) = mpsc::channel();
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP]).context("register signals")?;
    let signal_sender = sender.clone();
    thread::spawn(move || {
        for signal in signals.forever() {
            let event = if signal == SIGHUP {
                Event::Reload
            } else {
                Event::Shutdown
            };
            if signal_sender.send(event).is_err() {
                break;
            }
        }
    });
    thread::spawn(move || {
        if let Err(error) = monitor_suspend(sender) {
            warn!(%error, "logind suspend monitoring stopped");
        }
    });
    Ok(receiver)
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LoginManager {
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

fn monitor_suspend(sender: mpsc::Sender<Event>) -> Result<()> {
    let connection = zbus::blocking::Connection::system().context("connect to system D-Bus")?;
    let proxy = LoginManagerProxyBlocking::new(&connection).context("create logind proxy")?;
    let mut inhibitor = Some(
        proxy
            .inhibit(
                "sleep",
                "keylightd",
                "restore Key Lights before suspend",
                "delay",
            )
            .context("acquire logind sleep inhibitor")?,
    );
    for signal in proxy
        .receive_prepare_for_sleep()
        .context("subscribe to PrepareForSleep")?
    {
        let args = signal.args().context("decode PrepareForSleep")?;
        if args.start {
            let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
            if sender
                .send(Event::Suspend {
                    start: true,
                    ready: Some(ready_sender),
                })
                .is_err()
            {
                return Ok(());
            }
            let _ = ready_receiver.recv_timeout(Duration::from_secs(30));
            inhibitor.take();
        } else {
            if sender
                .send(Event::Suspend {
                    start: false,
                    ready: None,
                })
                .is_err()
            {
                return Ok(());
            }
            inhibitor = Some(
                proxy
                    .inhibit(
                        "sleep",
                        "keylightd",
                        "restore Key Lights before suspend",
                        "delay",
                    )
                    .context("reacquire logind sleep inhibitor")?,
            );
        }
    }
    Ok(())
}

enum LockOutcome {
    Acquired(UnixDatagram),
    Shutdown,
    Failed(std::io::Error),
}

fn wait_for_lock(shutdown: &AtomicBool) -> LockOutcome {
    wait_for_lock_named(LOCK_NAME, shutdown)
}

// Acquire the machine-wide single-writer lock, waiting for the current owner to
// release it rather than exiting on contention. This survives the restart race
// during an upgrade and lets ownership hand off between sessions, so whichever
// instance is parked here takes over when the previous owner logs out, suspends,
// or stops. The wait is interruptible so a stop request never blocks shutdown.
fn wait_for_lock_named(name: &[u8], shutdown: &AtomicBool) -> LockOutcome {
    let mut retry = Retry::immediate();
    let mut waited = false;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return LockOutcome::Shutdown;
        }
        match acquire_lock_named(name) {
            Ok(lock) => {
                if waited {
                    info!("acquired keylightd ownership after the previous owner released it");
                }
                return LockOutcome::Acquired(lock);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                if waited {
                    debug!(
                        retry_seconds = retry.backoff.as_secs(),
                        "still waiting for the keylightd ownership lock"
                    );
                } else {
                    waited = true;
                    info!(
                        retry_seconds = retry.backoff.as_secs(),
                        "another keylightd instance holds the machine-wide lock; \
                         waiting to take over ownership"
                    );
                }
                if wait_or_shutdown(retry.backoff, shutdown) {
                    info!(
                        "stopping before acquiring ownership; \
                         shutdown requested while another instance held the lock"
                    );
                    return LockOutcome::Shutdown;
                }
                retry = retry.failed();
            }
            Err(error) => return LockOutcome::Failed(error),
        }
    }
}

// Sleep for the given duration in small slices, returning early with true as
// soon as a shutdown is requested.
fn wait_or_shutdown(duration: Duration, shutdown: &AtomicBool) -> bool {
    const SLICE: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + duration;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep((deadline - now).min(SLICE));
    }
}

fn acquire_lock_named(name: &[u8]) -> std::io::Result<UnixDatagram> {
    let address = SocketAddr::from_abstract_name(name)?;
    UnixDatagram::bind_addr(&address)
}

fn journal_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Ok(Path::new(&path).join("keylightd/ownership.json"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(Path::new(&home).join(".local/state/keylightd/ownership.json"))
}

fn load_journal() -> Result<OwnershipJournal> {
    let path = journal_path()?;
    if !path.exists() {
        return Ok(OwnershipJournal::default());
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("read journal {}", path.display()))?;
    let journal: OwnershipJournal = match serde_json::from_str(&contents) {
        Ok(journal) => journal,
        Err(error) => {
            warn!(%error, path = %path.display(), "ignoring unreadable ownership journal");
            return Ok(OwnershipJournal::default());
        }
    };
    if journal.version != STATE_VERSION {
        warn!(
            version = journal.version,
            expected = STATE_VERSION,
            "ignoring ownership journal from an incompatible version"
        );
        return Ok(OwnershipJournal::default());
    }
    Ok(journal)
}

fn save_journal(journal: &OwnershipJournal) -> Result<()> {
    let path = journal_path()?;
    let parent = path.parent().context("journal has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create journal directory {}", parent.display()))?;
    if journal.lights.is_empty() {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("remove empty journal {}", path.display()))?;
        }
        return Ok(());
    }
    let temporary = parent.join(".ownership.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(journal).context("serialize ownership journal")?,
    )
    .with_context(|| format!("write temporary journal {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("publish ownership journal {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_is_capped() {
        let mut retry = Retry::immediate();
        for _ in 0..10 {
            retry = retry.failed();
        }
        assert_eq!(retry.backoff, MAX_RETRY);
    }

    #[test]
    fn abstract_socket_enforces_single_owner() {
        let name = format!("keylightd-test-{}", std::process::id());
        let first = acquire_lock_named(name.as_bytes()).unwrap();
        assert!(acquire_lock_named(name.as_bytes()).is_err());
        drop(first);
        assert!(acquire_lock_named(name.as_bytes()).is_ok());
    }

    #[test]
    fn wait_for_lock_acquires_when_free() {
        let name = format!("keylightd-free-{}", std::process::id());
        let shutdown = AtomicBool::new(false);
        match wait_for_lock_named(name.as_bytes(), &shutdown) {
            LockOutcome::Acquired(_lock) => {}
            _ => panic!("expected an immediate acquisition when the lock is free"),
        }
    }

    #[test]
    fn wait_for_lock_yields_when_already_shutting_down() {
        let name = format!("keylightd-preempt-{}", std::process::id());
        let shutdown = AtomicBool::new(true);
        match wait_for_lock_named(name.as_bytes(), &shutdown) {
            LockOutcome::Shutdown => {}
            _ => panic!("expected shutdown to preempt acquisition"),
        }
    }

    #[test]
    fn wait_for_lock_gives_up_on_shutdown_during_contention() {
        let name = format!("keylightd-contend-{}", std::process::id());
        let held = acquire_lock_named(name.as_bytes()).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let owned_name = name.clone();
        let waiter = thread::spawn(move || wait_for_lock_named(owned_name.as_bytes(), &flag));
        thread::sleep(Duration::from_millis(50));
        shutdown.store(true, Ordering::Relaxed);
        match waiter.join().unwrap() {
            LockOutcome::Shutdown => {}
            _ => panic!("expected the contended waiter to yield on shutdown"),
        }
        drop(held);
    }
}
