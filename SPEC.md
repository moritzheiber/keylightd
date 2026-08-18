# keylightd specification

## Purpose

`keylightd` controls one or more Elgato Key Lights from actual Linux camera frame activity. It samples ambient light once when a camera session starts, applies calibrated brightness, and restores each light's prior state when capture ends.

The target platform is Ubuntu 26.04 LTS with GNOME on Wayland. The design supports browsers using direct V4L2 access, PipeWire clients, and applications such as Zoom without depending on application-specific APIs.

## Goals

- Detect completed camera frames rather than open device handles.
- Support all attached physical V4L2 capture cameras by default.
- Allow explicit camera and Key Light selection.
- Control multiple physical Key Lights independently.
- Control every logical light returned by a selected device.
- Preserve and restore each logical light's original power and brightness.
- Survive temporary device, network, helper, daemon, and session failures.
- Keep privileged code isolated from network and user configuration.
- Remain usable without an ambient-light sensor or configuration file.
- Provide one containerized Debian build path for local and GitHub releases.

## Non-goals

- Color-temperature control.
- Reading or storing camera image data.
- Application-specific meeting detection.
- Guaranteeing restoration after power loss.
- Coordinating with third-party light controllers during normal operation.

## Process architecture

### Privileged camera monitor

`keylightd camera-monitor` runs as a system service because tracefs is root-only. It:

- enables only the `v4l2_dqbuf` tracepoint in a private tracefs instance;
- accepts only completed `VIDEO_CAPTURE` buffers with non-zero payloads;
- maps V4L2 minors to physical cameras;
- publishes camera identity, device nodes, last-frame time, and a heartbeat;
- never reads frame contents, user configuration, or network resources.

### Per-user controller

`keylightd daemon` runs as a user service because configuration, ownership state, mDNS, logind, and the Elgato API belong to the graphical user context. It:

- filters camera activity using the user's selection and timeouts;
- owns Key Light discovery and HTTP control;
- stores the restoration journal;
- handles reload, shutdown, suspend, and retry policy.

Only one user daemon may run per machine. A Linux abstract Unix socket provides a machine-wide advisory lock without shared writable filesystem state.

## Domain concepts

### Camera identity

The preferred identity is the udev `ID_SERIAL` value. `ID_PATH` and canonical sysfs paths are fallbacks. Multiple V4L2 interfaces with the same serial represent one physical camera.

The camera monitor discovers nodes whose V4L2 index is zero. Trace events still require `VIDEO_CAPTURE`, preventing metadata buffers from activating lights.

### Key Light identity

The preferred identity is the serial number returned by `/elgato/accessory-info`. MAC address is the fallback identity. mDNS service name and IP endpoint are resolution hints, not identity.

### Logical light state

A physical device may return multiple entries from `/elgato/lights`. Power and brightness are captured, overridden, and restored for every entry independently.

### Camera session

A session begins when any included camera has a recent completed frame. For fresh helper state, it ends when the larger of the camera inactivity timeout and restoration grace has elapsed since the final frame.

Ambient brightness is calculated once per session. A new camera appearing during the same aggregate session does not resample ambient light.

### Ownership

Before changing a light, the daemon records:

- stable device selection and resolution hints;
- every original logical state;
- every state keylightd intends to apply.

The journal is written before the override. A light remains owned until exact restoration succeeds or restart reconciliation proves that another controller changed it.

## Camera-state protocol

The root helper atomically writes JSON to `/run/keylightd/camera-state.json`.

```json
{
  "version": 1,
  "heartbeat_ms": 123456,
  "cameras": [
    {
      "id": "serial:046d_MX_Brio_2403LZ53W2R8",
      "name": "MX Brio",
      "devices": ["/dev/video4"],
      "last_frame_ms": 123400
    }
  ]
}
```

The protocol is versioned. Times use Linux `CLOCK_BOOTTIME`, not wall-clock time. This avoids clock-adjustment errors and includes suspend duration.

The snapshot is stale when its heartbeat is older than `helper_stale_seconds`. Each camera remains active until the larger of its configured inactivity timeout and the restoration grace expires after its last frame. Missing, invalid, or stale helper state starts a fresh restoration grace period.

## Configuration

The configuration path is `${XDG_CONFIG_HOME:-~/.config}/keylightd/config.toml`.

If the file is absent, built-in defaults apply. If exactly one Key Light is discovered, its stable selection is saved automatically. Multiple discovered lights require `keylightd lights select`.

An absent selection means automatic behavior. An explicitly empty selection is invalid because it is likely accidental configuration loss.

### Key Light selection

```toml
[[light.selected]]
id = "serial:A2UTB32614EYQG"
name = "Elgato Key Light MK.2"
service_name = "Elgato Key Light._elg._tcp.local."
fallback_address = "http://192.168.178.103:9123"
```

A configured fallback endpoint is tried first and must identify the selected hardware through accessory-info. mDNS is used when that verified endpoint is unavailable or identifies another device.

### Camera selection

Without `camera.selected`, all discovered physical capture cameras are included, including cameras attached later.

With an explicit selection, new cameras are ignored until selected.

```toml
[[camera.selected]]
id = "serial:046d_MX_Brio_2403LZ53W2R8"
name = "MX Brio"
inactive_seconds = 5
```

`inactive_seconds` is optional and defaults to `camera.default_inactive_seconds`.

### Calibration

Calibration points must:

- use finite, non-negative lux values;
- have unique, strictly increasing lux values;
- use brightness from 1 through 100;
- have monotonically non-decreasing brightness.

Values between points are linearly interpolated. Values outside the curve use the nearest endpoint.

Without valid sensor input or calibration, each logical light retains its current brightness and is only powered on.

### Reload

`keylightd reload` sends `SIGHUP` through the user systemd manager. The daemon atomically adopts a valid configuration. Invalid reloads are logged and the last valid configuration remains active.

Invalid configuration at initial startup is fatal.

## CLI convenience workflows

- `keylightd lights list`: scriptable tab-separated discovery output.
- `keylightd lights select`: interactive multi-selection saved to configuration.
- `keylightd cameras list`: scriptable tab-separated helper inventory.
- `keylightd cameras select`: interactive multi-selection saved to configuration.
- `keylightd discover`: resolve selected lights and show all logical states.
- `keylightd sensor`: show the current validated lux reading.
- `keylightd calibrate`: add or replace one calibration point and preview it on all selected lights.
- `keylightd reload`: reload the running user daemon.

Selection commands leave existing configuration unchanged when discovery returns no devices or the user submits a blank selection.

## Runtime policy

### Activation

- All included cameras are aggregated.
- Reachable selected lights activate immediately.
- Unreachable selected lights retry independently with capped exponential backoff.
- Each light snapshots its own logical states.
- Existing brightness is preserved when no session brightness is available.

### Restoration

- Fresh helper state overlaps camera inactivity and restoration grace instead of applying them consecutively.
- Missing, invalid, or stale helper state starts a fresh grace period.
- Camera activity before restoration cancels the pending transition.
- Camera activity during restoration reuses the original pre-first-session state for lights not yet restored.
- Restoration retries indefinitely with capped backoff.
- Manual changes during an uninterrupted daemon run are overwritten by exact restoration.

### Restart recovery

On restart, each journaled light is reconciled:

- if current state equals the last state applied by keylightd, ownership continues;
- if it differs, ownership is abandoned for that light to preserve the external change;
- if unreachable, reconciliation waits and the journal remains.

### Shutdown and logout

SIGTERM and SIGINT begin immediate restoration. The daemon exits after all owned lights restore. If systemd's stop timeout expires first, the process is killed and the journal remains for the next start.

### Suspend and resume

The daemon holds a logind delay inhibitor. On `PrepareForSleep(true)`, it restores owned lights and releases the inhibitor when complete. Logind may still enforce its maximum inhibitor delay when a light is unreachable.

Resume begins a new camera session; pre-suspend activity is not resumed implicitly.

## Edge-case decisions

1. Multiple discovered Key Lights require explicit selection.
2. A configured IP is a fallback for a selected hardware identity, never another identity.
3. IP changes are handled through mDNS rediscovery.
4. Unreachable lights retry independently.
5. New camera activity cancels restoration and retains original ownership.
6. Multiple cameras are aggregated.
7. Camera switching is masked by the larger of inactivity timeout and restoration grace.
8. Low-frame-rate cameras use per-camera inactivity timeouts.
9. Frozen streams become inactive because actual frames are authoritative.
10. Unwanted V4L2 devices can be excluded through explicit camera selection.
11. Camera activity already present at startup activates lights.
12. A user daemon starting after the helper reads the current snapshot.
13. An expired helper heartbeat is treated as inactive through normal grace.
14. Helper startup atomically replaces stale state.
15. Suspend restores lights and resume starts a new session.
16. Graceful logout restores lights.
17. Daemon crashes are recovered through the ownership journal.
18. Power loss cannot guarantee restoration.
19. Manual changes during a running session are overwritten on normal restoration.
20. Restart reconciliation preserves changes made by another controller.
21. Lights already on are restored to their exact prior states.
22. Every logical light entry is controlled and restored.
23. Missing sensor or calibration preserves current brightness.
24. Invalid lux is rejected.
25. Duplicate calibration lux values are rejected.
26. Decreasing calibration curves are rejected.
27. Configuration reload uses `SIGHUP`.
28. Corrupt startup configuration fails; corrupt reload retains the previous configuration.
29. A machine-wide abstract socket permits only the first user daemon.
30. Helper interruption during package upgrade follows normal heartbeat expiry and grace.

## Packaging and releases

`scripts/build-deb.sh` is the only packaging entry point. It uses Docker Buildx, the official Rust image, and an Ubuntu 26.04 packaging stage. The host receives only `dist/*.deb`.

Version tags matching the Cargo version, such as `v0.1.0`, build amd64 and arm64 packages and attach them to a GitHub release.

The Debian package:

- installs the binary under `/usr/bin`;
- installs system and user units under `/usr/lib/systemd`;
- enables and (re)starts the privileged camera service;
- enables the user controller globally and, best-effort, (re)starts it for any logged-in session.

Maintainer scripts follow the debhelper `dh_installsystemd` conventions. `deb-systemd-helper` manages enable, disable, and purge state; `deb-systemd-invoke` starts, stops, and restarts units; `systemctl --system daemon-reload` reloads the system manager. User-scoped actions run only when `DPKG_ROOT` is unset and are dispatched to each running `user@<uid>` manager through `deb-systemd-invoke --user`.

## Extension rules

- Keep frame observation privileged and all policy unprivileged.
- Add domain invariants as tests before adapter changes.
- Never infer identity from IP address.
- Persist restoration intent before changing hardware.
- Keep state files atomic and versionable.
- Reject invalid configuration rather than partially applying it.
- Preserve independent retry and ownership per physical light.
- Treat camera contents as out of scope; only trace metadata may be consumed.
- Update this specification when changing a behavioral decision or protocol.

## Known hardware status

The target ThinkPad X1 Carbon Gen 13 currently exposes no Linux IIO ambient-light device and `iio-sensor-proxy` reports no sensor. The architecture therefore keeps ambient sensing pluggable and treats current-brightness preservation as a supported operating mode.
