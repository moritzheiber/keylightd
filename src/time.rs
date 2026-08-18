use anyhow::{Context, Result};

pub fn boottime_ms() -> Result<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read CLOCK_BOOTTIME");
    }
    let seconds: u64 = value
        .tv_sec
        .try_into()
        .context("CLOCK_BOOTTIME returned negative seconds")?;
    let nanos: u64 = value
        .tv_nsec
        .try_into()
        .context("CLOCK_BOOTTIME returned negative nanoseconds")?;
    seconds
        .checked_mul(1_000)
        .and_then(|millis| millis.checked_add(nanos / 1_000_000))
        .context("CLOCK_BOOTTIME exceeds u64 milliseconds")
}
