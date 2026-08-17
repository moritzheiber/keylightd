use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info};

const FRAME_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
const CHECK_INTERVAL: Duration = Duration::from_millis(250);

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
        ctrlc::set_handler(move || signal_running.store(false, Ordering::Relaxed))
            .context("install signal handler")?;

        let instance = TraceInstance::create(&self.tracefs)?;
        publish_state(&self.state_file, false)?;
        let (sender, receiver) = mpsc::channel();
        let trace_pipe = instance.path.join("trace_pipe");
        thread::spawn(move || {
            if let Err(error) = read_frames(&trace_pipe, sender) {
                tracing::error!(%error, "camera trace reader stopped");
            }
        });

        let mut active = false;
        let mut last_frame = None;
        while running.load(Ordering::Relaxed) {
            match receiver.recv_timeout(CHECK_INTERVAL) {
                Ok(()) => {
                    last_frame = Some(Instant::now());
                    if !active {
                        active = true;
                        publish_state(&self.state_file, true)?;
                        info!("camera frames detected");
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if active && last_frame.is_some_and(|seen| seen.elapsed() >= FRAME_IDLE_TIMEOUT)
                    {
                        active = false;
                        publish_state(&self.state_file, false)?;
                        info!("camera frames stopped");
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("camera trace reader disconnected");
                }
            }
        }
        debug!("camera monitor stopped");
        Ok(())
    }
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

fn read_frames(trace_pipe: &Path, sender: mpsc::Sender<()>) -> Result<()> {
    let file = File::open(trace_pipe).with_context(|| format!("open {}", trace_pipe.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).context("read camera trace")? == 0 {
            continue;
        }
        if is_completed_capture_frame(&line) && sender.send(()).is_err() {
            return Ok(());
        }
    }
}

fn is_completed_capture_frame(line: &str) -> bool {
    if !line.contains("v4l2_dqbuf:")
        || !line.contains("type = VIDEO_CAPTURE")
        || line.contains("bytesused = 0,")
    {
        return false;
    }
    line.split("bytesused = ")
        .nth(1)
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|bytes| bytes > 0)
}

fn publish_state(path: &Path, active: bool) -> Result<()> {
    let parent = path.parent().context("camera state file has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create camera state directory {}", parent.display()))?;
    let temporary = parent.join(".camera-active.tmp");
    let mut file = File::create(&temporary)
        .with_context(|| format!("create temporary state {}", temporary.display()))?;
    writeln!(file, "{}", if active { "active" } else { "inactive" })
        .context("write camera state")?;
    fs::rename(&temporary, path).with_context(|| format!("publish camera state {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_completed_video_frames() {
        assert!(is_completed_capture_frame(
            "v4l2_dqbuf: minor = 4, type = VIDEO_CAPTURE, bytesused = 94879, sequence = 1"
        ));
        assert!(!is_completed_capture_frame(
            "v4l2_dqbuf: minor = 4, type = VIDEO_CAPTURE, bytesused = 0, sequence = 1"
        ));
        assert!(!is_completed_capture_frame(
            "v4l2_qbuf: minor = 4, type = VIDEO_CAPTURE, bytesused = 94879, sequence = 1"
        ));
    }
}
