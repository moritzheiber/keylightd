# keylightd

Turns an Elgato Key Light on when any V4L2 camera produces frames, chooses brightness and colour temperature from ambient-light calibration or a saved preset, and restores the previous power, brightness, and colour-temperature state five seconds after capture stops.

See [SPEC.md](SPEC.md) for behavioral decisions, architecture, edge cases, and extension guidance.

## Install

Download the package for your architecture from a GitHub release, then install it:

```console
sudo apt install ./keylightd_0.3.1_amd64.deb
```

Installation enables and starts the privileged camera service and enables the per-user controller globally, so the controller starts in the current graphical session and on each subsequent login.

Configuration is optional. With no config file the daemon uses defaults and auto-selects a single discovered Key Light. To start from the documented example, copy it into place and reload:

```console
install -Dm 0644 /usr/share/doc/keylightd/config.example.toml ~/.config/keylightd/config.toml
keylightd reload
```

The privileged service reads only the kernel V4L2 dequeue tracepoint and publishes a heartbeat plus per-camera frame state under `/run/keylightd`. The user service owns device selection, Key Light control, and restoration.

### Desktop applet (optional)

The `gnome-shell-extension-keylightd` package adds a dedicated GNOME Shell top-bar button, separate from the system Quick Settings menu. Its bulb icon tints with camera state. The dropdown holds one control group per light, each with a power toggle, a brightness slider showing a live percentage, and an expander revealing a colour-temperature slider showing a live Kelvin value. Two side-by-side tiles save and apply the preset.

```console
sudo apt install ./gnome-shell-extension-keylightd_0.3.1_all.deb
gnome-extensions enable keylightd@heiber.im
```

The button appears only while the `keylightd` session service is available on the bus. A freshly installed extension is loaded at the next login, matching GNOME's own behaviour. Global shortcuts for brightness up, brightness down, and power toggle are unset by default; bind them under the extension's settings.

## Build a package

The local builder requires only Docker with Buildx. The Rust toolchain is pinned by `rust-toolchain.toml` and installed with `rustup` inside the Ubuntu 26.04 build stage, where `dpkg-buildpackage` drives debhelper to compile and assemble the package (including cross-compilation):

```console
scripts/build-deb.sh
sudo apt install ./dist/keylightd_0.3.1_amd64.deb ./dist/gnome-shell-extension-keylightd_0.3.1_all.deb
```

By default `scripts/build-deb.sh` produces both the daemon and the extension package. Use `--architecture arm64` to cross-compile an arm64 daemon, and `--build any` or `--build all` to build only the daemon or only the extension. Pushing a tag matching the Cargo version, such as `v0.3.1`, runs the same containerized builder for the amd64 and arm64 daemon packages and the architecture-independent extension, attaching all of them to a GitHub release.

## Configure

Use `keylightd lights list` and `keylightd lights select` to choose one or more lights. Use `keylightd cameras list` and `keylightd cameras select` to restrict camera activation. Without an explicit camera selection, all detected capture cameras are included.

`keylightd sensor` prints the current IIO ambient-light reading. Run `keylightd calibrate` under several lighting conditions to save lux-to-brightness points; intermediate values are linearly interpolated. Without a sensor or calibration, camera activation preserves the Key Light's existing brightness.

Run `keylightd reload` after editing configuration. Set `RUST_LOG=keylightd=debug` for additional journal output.

## Limitations

Key Lights are controlled over the unencrypted Elgato local API on port `9123`. No TLS backend is compiled in, so only `http://` endpoints work; `https://` endpoints are rejected. This matches the devices, which expose no HTTPS listener, and keeps the release binary small by avoiding an asynchronous runtime and bundled cryptography libraries.

## Development

The daemon builds with the Rust toolchain pinned by `rust-toolchain.toml`.

```console
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

The GNOME Shell extension runs in a nested shell through Devkit. The `mutter-dev-bin` package provides `/usr/libexec/mutter-devkit`. The extension is visible only while `keylightd` owns the bus name, and the single-writer lock is machine-wide, so the system service must be stopped while a daemon runs on the private session bus.

```console
sudo apt install mutter-dev-bin
systemctl --user stop keylightd.service
dbus-run-session -- bash -c 'keylightd daemon & gnome-shell --wayland --devkit'
systemctl --user start keylightd.service
```

Local extension changes load by copying `extension/` into `~/.local/share/gnome-shell/extensions/keylightd@heiber.im/`, running `glib-compile-schemas` on its `schemas/`, and relaunching the nested shell. Global keyboard shortcuts do not reach a nested compositor.
