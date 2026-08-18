use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use tracing::{debug, info};

use crate::domain::{CameraObservation, CameraSnapshot, STATE_VERSION};
use crate::time::boottime_ms;

const CHECK_INTERVAL: Duration = Duration::from_millis(250);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

pub struct CameraMonitor {
    tracefs: PathBuf,
    state_file: PathBuf,
}

impl CameraMonitor {
    pub fn new(tracefs: PathBuf, state_file: PathBuf) -> Self {
        Self {
            tracefs,
            state_file,
        }
    }

    pub fn run(self) -> Result<()> {
        if unsafe { libc::geteuid() } != 0 {
            bail!("camera-monitor must run as root");
        }
        let running = Arc::new(AtomicBool::new(true));
        let signal_running = Arc::clone(&running);
        let mut signals = Signals::new([SIGINT, SIGTERM]).context("register signals")?;
        thread::spawn(move || {
            if signals.forever().next().is_some() {
                signal_running.store(false, Ordering::Relaxed);
            }
        });

        let instance = TraceInstance::create(&self.tracefs)?;
        let (sender, receiver) = mpsc::channel();
        let trace_pipe = instance.path.join("trace_pipe");
        thread::spawn(move || {
            if let Err(error) = read_frames(&trace_pipe, sender) {
                tracing::error!(%error, "camera trace reader stopped");
            }
        });

        let mut cameras = scan_cameras()?;
        let mut last_publish = Instant::now() - HEARTBEAT_INTERVAL;
        publish_snapshot(&self.state_file, &cameras)?;
        while running.load(Ordering::Relaxed) {
            let mut changed = false;
            match receiver.recv_timeout(CHECK_INTERVAL) {
                Ok(minor) => {
                    if let Some(camera) = cameras
                        .values_mut()
                        .find(|camera| camera.minors.contains(&minor))
                    {
                        let was_inactive = camera.last_frame_ms.is_none();
                        camera.last_frame_ms = Some(boottime_ms()?);
                        changed = true;
                        if was_inactive {
                            info!(
                                camera_id = camera.id,
                                camera_name = camera.name,
                                "camera frames detected"
                            );
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("camera trace reader disconnected");
                }
            }
            if last_publish.elapsed() >= HEARTBEAT_INTERVAL {
                cameras = merge_scan(cameras, scan_cameras()?);
                changed = true;
            }
            if changed {
                publish_snapshot(&self.state_file, &cameras)?;
                last_publish = Instant::now();
            }
        }
        debug!("camera monitor stopped");
        Ok(())
    }
}

#[derive(Clone)]
struct CameraDevice {
    id: String,
    name: String,
    devices: Vec<String>,
    minors: Vec<u32>,
    last_frame_ms: Option<u64>,
}

fn scan_cameras() -> Result<BTreeMap<String, CameraDevice>> {
    let mut cameras: BTreeMap<String, CameraDevice> = BTreeMap::new();
    let root = Path::new("/sys/class/video4linux");
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let path = entry.context("read video device entry")?.path();
        if fs::read_to_string(path.join("index"))
            .unwrap_or_default()
            .trim()
            != "0"
        {
            continue;
        }
        let device = format!("/dev/{}", entry_name(&path)?);
        let dev = fs::read_to_string(path.join("dev"))
            .with_context(|| format!("read device number for {device}"))?;
        let minor = dev
            .trim()
            .split(':')
            .nth(1)
            .context("video device number has no minor")?
            .parse::<u32>()
            .context("parse video device minor")?;
        let properties = read_udev_properties(dev.trim())?;
        let name = properties
            .get("ID_V4L_PRODUCT")
            .cloned()
            .or_else(|| {
                fs::read_to_string(path.join("name"))
                    .ok()
                    .map(|v| v.trim().to_owned())
            })
            .unwrap_or_else(|| device.clone());
        let id = camera_identity(&properties, &path)?;
        let camera = cameras.entry(id.clone()).or_insert_with(|| CameraDevice {
            id,
            name,
            devices: Vec::new(),
            minors: Vec::new(),
            last_frame_ms: None,
        });
        camera.devices.push(device);
        camera.minors.push(minor);
    }
    Ok(cameras)
}

fn merge_scan(
    previous: BTreeMap<String, CameraDevice>,
    mut current: BTreeMap<String, CameraDevice>,
) -> BTreeMap<String, CameraDevice> {
    for (id, camera) in &mut current {
        camera.last_frame_ms = previous.get(id).and_then(|old| old.last_frame_ms);
    }
    current
}

fn entry_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .context("video device has no valid name")
}

fn read_udev_properties(dev: &str) -> Result<HashMap<String, String>> {
    let path = Path::new("/run/udev/data").join(format!("c{dev}"));
    let contents = fs::read_to_string(&path).unwrap_or_default();
    Ok(parse_udev_properties(&contents))
}

fn parse_udev_properties(contents: &str) -> HashMap<String, String> {
    contents
        .lines()
        .filter_map(|line| line.strip_prefix("E:")?.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn camera_identity(properties: &HashMap<String, String>, sysfs_path: &Path) -> Result<String> {
    properties
        .get("ID_SERIAL")
        .map(|serial| format!("serial:{serial}"))
        .or_else(|| properties.get("ID_PATH").map(|path| format!("path:{path}")))
        .or_else(|| {
            fs::canonicalize(sysfs_path)
                .ok()
                .map(|path| format!("sysfs:{}", path.display()))
        })
        .context("camera has no stable identity")
}

fn publish_snapshot(path: &Path, cameras: &BTreeMap<String, CameraDevice>) -> Result<()> {
    let snapshot = CameraSnapshot {
        version: STATE_VERSION,
        heartbeat_ms: boottime_ms()?,
        cameras: cameras
            .values()
            .map(|camera| CameraObservation {
                id: camera.id.clone(),
                name: camera.name.clone(),
                devices: camera.devices.clone(),
                last_frame_ms: camera.last_frame_ms,
            })
            .collect(),
    };
    let contents = serde_json::to_vec_pretty(&snapshot).context("serialize camera snapshot")?;
    atomic_write(path, &contents)
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("state file has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create state directory {}", parent.display()))?;
    let temporary = parent.join(".camera-state.tmp");
    fs::write(&temporary, contents)
        .with_context(|| format!("write temporary state {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("publish state {}", path.display()))
}

struct TraceInstance {
    path: PathBuf,
}

impl TraceInstance {
    fn create(tracefs: &Path) -> Result<Self> {
        let path = tracefs.join("instances/keylightd");
        if path.exists() {
            fs::remove_dir(&path)
                .with_context(|| format!("remove stale trace instance {}", path.display()))?;
        }
        fs::create_dir(&path)
            .with_context(|| format!("create trace instance {}", path.display()))?;
        fs::write(path.join("trace"), "").context("clear camera trace")?;
        fs::write(path.join("events/v4l2/v4l2_dqbuf/enable"), "1")
            .context("enable V4L2 dequeue tracepoint")?;
        fs::write(path.join("tracing_on"), "1").context("start camera tracing")?;
        Ok(Self { path })
    }
}

impl Drop for TraceInstance {
    fn drop(&mut self) {
        let _ = fs::write(self.path.join("tracing_on"), "0");
        let _ = fs::write(self.path.join("events/v4l2/v4l2_dqbuf/enable"), "0");
        let _ = fs::remove_dir(&self.path);
    }
}

fn read_frames(trace_pipe: &Path, sender: mpsc::Sender<u32>) -> Result<()> {
    let file = File::open(trace_pipe).with_context(|| format!("open {}", trace_pipe.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).context("read camera trace")? == 0 {
            continue;
        }
        if let Some(minor) = completed_capture_minor(&line)
            && sender.send(minor).is_err()
        {
            return Ok(());
        }
    }
}

fn completed_capture_minor(line: &str) -> Option<u32> {
    if !line.contains("v4l2_dqbuf:")
        || !line.contains("type = VIDEO_CAPTURE")
        || line.contains("bytesused = 0,")
    {
        return None;
    }
    let bytes = field_value(line, "bytesused = ")?.parse::<u64>().ok()?;
    if bytes == 0 {
        return None;
    }
    field_value(line, "minor = ")?.parse().ok()
}

fn field_value<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    line.split(field).nth(1)?.split(',').next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_completed_video_frame_minor() {
        assert_eq!(
            completed_capture_minor(
                "v4l2_dqbuf: minor = 4, type = VIDEO_CAPTURE, bytesused = 94879, sequence = 1"
            ),
            Some(4)
        );
        assert_eq!(
            completed_capture_minor(
                "v4l2_dqbuf: minor = 4, type = VIDEO_CAPTURE, bytesused = 0, sequence = 1"
            ),
            None
        );
    }

    #[test]
    fn parses_udev_identity_properties() {
        let properties =
            parse_udev_properties("E:ID_SERIAL=046d_MX_Brio_123\nE:ID_V4L_PRODUCT=MX Brio\n");
        assert_eq!(properties["ID_SERIAL"], "046d_MX_Brio_123");
    }
}
