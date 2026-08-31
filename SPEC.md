# keylightd specification

## Purpose

`keylightd` controls one or more Elgato Key Lights from actual Linux camera frame activity. When a camera session starts it chooses each light's target from ambient calibration, a saved preset, or the current state, and restores each light's prior state when capture ends.

The target platform is Ubuntu 26.04 LTS with GNOME on Wayland. The design supports browsers using direct V4L2 access, PipeWire clients, and applications such as Zoom without depending on application-specific APIs.

## Goals

- Detect completed camera frames rather than open device handles.
- Support all attached physical V4L2 capture cameras by default.
- Allow explicit camera and Key Light selection.
- Control multiple physical Key Lights independently.
- Control every logical light returned by a selected device.
- Preserve and restore each logical light's original power, brightness, and color temperature.
- Survive temporary device, network, helper, daemon, and session failures.
- Keep privileged code isolated from network and user configuration.
- Remain usable without an ambient-light sensor or configuration file.
- Offer an optional desktop status and control surface without weakening automatic restoration.
- Provide one containerized Debian build path for local and GitHub releases.

## Non-goals

- Reading or storing camera image data.
- Application-specific meeting detection.
- Guaranteeing restoration after power loss.
- Coordinating with third-party light controllers during normal operation.
- Multiple named presets; a single preset is stored.
- Per-light presets; the preset captures all selected lights together.

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

Only one user daemon may run per machine. A Linux abstract Unix socket provides a machine-wide advisory lock without shared writable filesystem state. The kernel releases the name automatically when the owning process exits, by any means, so the lock cannot go stale and needs no cleanup. A daemon that finds the lock already held does not exit; it waits and retries with exponential backoff (capped) until the current owner releases it or a stop signal arrives, so ownership hands off cleanly between sessions and survives the brief socket-release race during a restart. The wait is interruptible, so a stop request never blocks shutdown.

## Domain concepts

### Camera identity

The preferred identity is the udev `ID_SERIAL` value. `ID_PATH` and canonical sysfs paths are fallbacks. Multiple V4L2 interfaces with the same serial represent one physical camera.

The camera monitor discovers nodes whose V4L2 index is zero. Trace events still require `VIDEO_CAPTURE`, preventing metadata buffers from activating lights.

### Key Light identity

The preferred identity is the serial number returned by `/elgato/accessory-info`. MAC address is the fallback identity. mDNS service name and IP endpoint are resolution hints, not identity.

### Logical light state

A physical device may return multiple entries from `/elgato/lights`. Power, brightness, and color temperature are captured, overridden, and restored for every entry independently.

Color temperature is a first-class controlled dimension. It is exposed and configured in Kelvin over the range 2900 K through 7000 K and converted to the device's mired units internally. The device range is 143 mired (7000 K, cool) through 344 mired (2900 K, warm); conversion is `mired = round(1000000 / kelvin)`, clamped to that range.

### Key Light transport

The Elgato local API is unencrypted HTTP on port `9123`. Control uses a blocking `ureq` client with a two-second connect timeout and a three-second global timeout: `GET /elgato/accessory-info` for identity, `GET /elgato/lights` for state, and `PUT /elgato/lights` to apply state. Each logical light carries `on`, `brightness`, and `temperature`; the transport reads and writes all three. Non-2xx responses are treated as errors.

Only plaintext HTTP is supported. No TLS backend is compiled in, so `https://` endpoints are rejected at request time. This is deliberate. The devices expose no HTTPS listener, and omitting TLS removes the asynchronous runtime and bundled cryptography libraries that a general-purpose HTTP client would otherwise pull in.

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

Calibration determines brightness only. It does not set color temperature; a future ambient warmth curve may extend it.

### Preset

A single preset stores a saved look for all selected lights. It captures, per logical light, the complete state: `on`, `brightness`, and `temperature`. It is persisted in configuration.

```toml
[[light.preset]]
id = "serial:A2UTB32614EYQG"
on = true
brightness = 60
temperature_kelvin = 4500
```

`keylightd lights save-preset` captures the current state of every selected light. `keylightd lights apply-preset` reproduces the saved state on every selected light.

Applying a preset is faithful rather than a blanket power-on. A light saved as off is set off, and lights saved on are set to their saved brightness and temperature. Applying a preset is a manual action and follows manual-control semantics.

### Camera-on target

When a camera session starts, each selected light's target is chosen by precedence:

1. Calibrated brightness, when a valid sensor reading and calibration exist. Color temperature is left unchanged unless a warmth curve is configured.
2. The saved preset, when one exists and calibration does not apply. Each light is set to its saved `on`, `brightness`, and `temperature`.
3. Preserve current otherwise, so each light retains its current brightness and is powered on.

An all-off preset is applied faithfully, so camera-on may leave lights off and make no visible change. Before any preset is saved, camera-on preserves current state and powers lights on. Camera-off is unaffected by target selection and always restores the pre-session snapshot.

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
- `keylightd lights save-preset`: capture the current state of all selected lights as the preset.
- `keylightd lights apply-preset`: apply the saved preset to all selected lights.
- `keylightd reload`: reload the running user daemon.

Selection commands leave existing configuration unchanged when discovery returns no devices or the user submits a blank selection.

## Runtime policy

### Activation

- All included cameras are aggregated.
- Reachable selected lights activate immediately.
- Unreachable selected lights retry independently with capped exponential backoff.
- Each light snapshots its own logical state, including color temperature.
- The activation target follows the camera-on precedence of calibration, then preset, then preserve current.

### Restoration

- Fresh helper state overlaps camera inactivity and restoration grace instead of applying them consecutively.
- Missing, invalid, or stale helper state starts a fresh grace period.
- Camera activity before restoration cancels the pending transition.
- Camera activity during restoration reuses the original pre-first-session state for lights not yet restored.
- Restoration retries indefinitely with capped backoff.
- Manual changes during an uninterrupted daemon run are overwritten by exact restoration.
- Manual control mediated by the daemon never alters the pre-session snapshot; camera-off restores that snapshot unconditionally.

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
29. A machine-wide abstract socket admits one user daemon at a time; a second instance waits and takes over when the current owner releases the lock.
30. Helper interruption during package upgrade follows normal heartbeat expiry and grace.
31. Manual control is mediated by the daemon and never contacts a light directly.
32. Manual control never changes the pre-session snapshot; camera-off restores it unconditionally.
33. Manual changes during a session update the applied state so the daemon does not fight them.
34. Manual changes with no active session become the snapshot the next session restores.
35. Color temperature is captured, applied, and restored with power and brightness.
36. Color temperature is exposed in Kelvin and converted to device mired internally.
37. Saving a preset captures every selected light's power, brightness, and temperature.
38. Applying a preset reproduces saved state faithfully; a light saved off stays off.
39. Camera-on target precedence is calibration, then preset, then preserve current.
40. An all-off preset applied on camera-on leaves lights off and makes no visible change.
41. Each selected light is a separate D-Bus object so the applet renders one row per light.

## Interactive control surface

This section specifies the optional desktop status and control surface. It is delivered as a GNOME Shell extension backed by a session-bus interface on the user controller. It does not change automatic behavior. The pre-session snapshot is captured only at camera-session start and consumed only at restoration, so camera-off always restores the state that preceded the session regardless of manual input.

### Session D-Bus interface

The user controller exports a session-bus service. The well-known name is `im.heiber.keylightd` and the root path is `/im/heiber/keylightd`, following the reverse-DNS D-Bus convention. The controller defines the interfaces with zbus and is the authoritative source; the introspection XML is exported to the source tree so the extension builds its proxies from the same contract.

The service is served from a dedicated thread on a blocking zbus connection, mirroring the logind listener. No asynchronous runtime is introduced.

The root object implements `im.heiber.keylightd1`. Its properties are read-only and change-signalled through the standard `PropertiesChanged`:

- `CameraActive` (`b`): a camera session is active. This drives the panel status icon.
- `HasPreset` (`b`): a preset is saved.

Root methods operate on the whole selected set:

- `SavePreset()`: capture every selected light's current state as the preset.
- `ApplyPreset()`: apply the saved preset to every selected light, reproducing each light's saved power, brightness, and temperature faithfully.

Each selected logical light is exported as a child object at `/im/heiber/keylightd/light/<n>` implementing `im.heiber.keylightd1.Light`, so the extension renders one control row per light. Its properties are read-only and change-signalled:

- `Id` (`s`): stable hardware identity.
- `Name` (`s`): the device display name. It tracks the mobile-app name, refreshed from the device on each poll, so renaming the light in the Elgato app updates the applet within one poll. The last known name is kept while the light is unreachable.
- `On` (`b`): power state.
- `Brightness` (`y`): brightness, 1 through 100.
- `TemperatureKelvin` (`q`): color temperature in Kelvin, 2900 through 7000.
- `Reachable` (`b`): the light is reachable.

Per-light methods apply only to that light:

- `SetPower(b)`, `TogglePower()`.
- `SetBrightness(y)` and `AdjustBrightness(i)`, clamped to range.
- `SetTemperatureKelvin(q)` and `AdjustTemperatureKelvin(i)`, clamped to range.

Kelvin is exposed on the interface and converted to device mired internally, so clients never handle mired.

### Manual control semantics

Manual commands are always mediated by the controller; the extension never contacts a light directly. This preserves the single-writer model and the machine-wide single-owner lock. Applying a preset is a manual action and follows these rules.

- The pre-session snapshot is captured only at camera-session start and is never mutated by manual control.
- During an active session, manual commands change the live light and the journaled applied state, so the controller does not fight them and restart reconciliation stays consistent. Restoration at camera-off still targets the snapshot.
- With no active session, manual commands set the light's resting state. That resting state becomes the snapshot captured by the next session and therefore the state restored at its end.

Camera-off restoration is unconditional. It always returns the selected lights to the snapshot captured at session start.

### GNOME Shell extension

The extension is a standalone panel button, not an entry in the shared system Quick Settings menu. It is a `PanelMenu.Button` added to the panel's right box with `Main.panel.addToStatusArea`, so it owns a separate top-bar icon and its own dropdown `PopupMenu`; nothing is injected into the aggregated system menu.

The panel button's icon is a bulb whose tint reflects `CameraActive`, accent-tinted while a camera session drives the lights and neutral while idle. The icon is a bundled symbolic SVG loaded from the extension's `icons/` directory, as is the thermometer used for colour temperature; both recolour like theme symbolics.

The dropdown renders one control group per light object, each introduced by a section header carrying the light's name. Within a group a single brightness row places a power-toggle icon button, a brightness `Slider`, a value readout, and a trailing chevron; the chevron toggles an inline colour-temperature row directly below it, holding a thermometer icon that tints warm to cool with the selected Kelvin, a temperature `Slider`, and its own readout. Each readout shows the brightness percentage or the colour temperature in Kelvin, computed from the same slider-to-device conversion the setters use so the number always matches what is applied, and it updates live while dragging. The leading icon and trailing chevron of both rows occupy fixed-width slots and the readouts share a fixed width, so the two sliders line up exactly. The power icon dims while the light is off. Below the light groups, two always-visible tiles sit side by side, save preset and apply preset, mapping to the root `SavePreset` and `ApplyPreset` methods; apply is disabled unless `HasPreset` is true.

Slider changes are rate limited before they reach the bus. Dragging a slider emits a value notification on every motion step, so each control coalesces them. The first change is sent immediately, further changes are sent at most once per interval, and the settled value is flushed when the drag ends. While a slider is being dragged, incoming property updates do not snap its handle, so live status never fights the user's motion. This keeps one drag from turning into a flood of D-Bus calls and Key Light HTTP requests.

The whole menu is populated once the root proxy is ready, so light rows that depend on the asynchronously fetched `LightPaths` are added reliably rather than racing the button's insertion into the panel.

The extension reads properties and subscribes to `PropertiesChanged` for live status, and calls interface methods for all control. It holds no light state of its own.

The extension UUID is `keylightd@heiber.im`, following the GJS convention of `name@domain-under-your-control` in normal domain order. It declares a supported `shell-version` set and is enabled per user; installation does not enable it automatically.

Global hotkeys for brightness up, brightness down, and power toggle are registered by the extension through a GSettings schema and the shell keybinding API. Under Wayland only the compositor can bind global keys, so the extension is the only correct place for them; each binding calls an interface method.

## Packaging and releases

The source tree produces two binary packages from one debhelper source package:

- `keylightd` (`Architecture: any`): the daemon, camera monitor, system and user units, manpage, and documentation.
- `gnome-shell-extension-keylightd` (`Architecture: all`): the extension and its GSettings schema. It depends on `keylightd (= ${binary:Version})` and on a `gnome-shell` version range derived from the extension's `metadata.json` (`>= min~`, `<< max+1~`), following the Debian GNOME team convention.

`debian/control` declares both stanzas. `debian/rules` uses `dh $@` with architecture-split overrides, so the Cargo build and binary install run only for the architecture-dependent daemon, while the architecture-independent extension ships data through its `debian/<package>.install` file. The supported GNOME Shell major range is extracted from `metadata.json` in `debian/rules` with coreutils alone and passed to `dh_gencontrol` as substvars, so no scripting-language interpreter enters `Build-Depends`. `dh_installsystemd` generates the systemd enable, reload, and restart snippets for the system and user units, and `dh_installgsettings` adds the settings-backend dependency; the schema is recompiled on install by the glib schemas trigger. No maintainer-script logic is hand-written, matching how the Debian GNOME team packages shell extensions.

Packages are built inside the `ubuntu:26.04` container with `dpkg-buildpackage`: `--build=any` per architecture for the daemon, and `--build=all` once for the architecture-independent extension. The host receives only the resulting `.deb` files.

The Rust toolchain is defined solely by `rust-toolchain.toml` (channel, profile, components, and cross targets). The container installs `rustup` from Ubuntu and materialises the pinned toolchain with `rustup show`, so the compiler version has a single source of truth and no separate image tag to keep in sync.

Version tags matching the Cargo version, such as `v0.1.0`, build both packages and attach them to a GitHub release. The release workflow reuses the CI workflow (`.github/workflows/ci.yml`) for formatting, linting, and tests before packaging.

The user controller is enabled globally and started for logged-in sessions, and the privileged camera service is enabled and started. The extension is enabled per user: GNOME Shell scans the extension directories only at session startup and has no filesystem monitor, so a freshly installed extension becomes available at the next login and is then enabled with `gnome-extensions enable`. The package therefore ships no maintainer scripts to load or enable it, matching the Debian GNOME team's shell-extension packaging.

The release profile is tuned for binary size: `opt-level = "z"`, fat LTO, a single codegen unit, `panic = "abort"`, and symbol stripping. Combined with the plaintext-HTTP transport, this keeps the binary small; no separate debug package is shipped.

## Extension rules

- Keep frame observation privileged and all policy unprivileged.
- Add domain invariants as tests before adapter changes.
- Never infer identity from IP address.
- Persist restoration intent before changing hardware.
- Keep state files atomic and versionable.
- Reject invalid configuration rather than partially applying it.
- Preserve independent retry and ownership per physical light.
- Treat camera contents as out of scope; only trace metadata may be consumed.
- Keep the Key Light transport blocking and plaintext HTTP; do not reintroduce an asynchronous runtime or TLS for devices that expose neither.
- Route all manual light control through the daemon; keep it the single writer and never contact a light from the extension.
- Never mutate the pre-session snapshot from manual control; camera-off restores it unconditionally.
- Keep the controller the authoritative D-Bus contract source; export its introspection XML for the extension and keep them in sync.
- Store and transport colour temperature in device mired for faithful capture and restore; expose and accept Kelvin only at the user-facing boundaries (configuration, CLI, D-Bus).
- Apply presets faithfully per light; never coerce a saved-off light on.
- Build the binary packages from one debhelper source package; do not hand-assemble packages.
- Update this specification when changing a behavioral decision or protocol.

## Known hardware status

The target ThinkPad X1 Carbon Gen 13 currently exposes no Linux IIO ambient-light device and `iio-sensor-proxy` reports no sensor. The architecture therefore keeps ambient sensing pluggable and treats current-brightness preservation as a supported operating mode. On such hardware a saved preset is the primary way to get a consistent on-camera look, with calibration reserved for systems that do expose a sensor.
