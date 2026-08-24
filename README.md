# keylightd

Turns an Elgato Key Light on when any V4L2 camera produces frames, chooses brightness and colour temperature from ambient-light calibration or a saved preset, and restores the previous power, brightness, and colour-temperature state five seconds after capture stops.

See [SPEC.md](SPEC.md) for behavioral decisions, architecture, edge cases, and extension guidance.

## Install

On Ubuntu 26.04 (resolute), add the signed APT repository and install from it. This keeps the package updated with `apt upgrade`.

```console
curl -fsSL https://moritzheiber.github.io/keylightd/keylightd.gpg | sudo tee /etc/apt/keyrings/keylightd.gpg > /dev/null
sudo curl -fsSL https://moritzheiber.github.io/keylightd/keylightd.sources -o /etc/apt/sources.list.d/keylightd.sources
sudo apt update
sudo apt install keylightd
```

The archive signing key fingerprint is `64C3 3EBB 1F9F E944 F27B 19B6 C429 6BDA D3AC 2EDB`. Verify it with `gpg --show-keys /etc/apt/keyrings/keylightd.gpg`.

Alternatively, download the package for your architecture from a GitHub release and install it directly:

```console
sudo apt install ./keylightd_0.4.0_amd64.deb
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

### Releasing

`scripts/bump-version.sh` sets the version in `Cargo.toml`, `Cargo.lock` and `debian/changelog` together, then commits or amends, tags `vX.Y.Z`, and pushes. It prompts for the target version, the changelog entries, and each git action, signing the commit and tag when git is configured for it. Before committing it re-runs the release workflow's own guard locally, so the tag, the Cargo version and the changelog version are verified to agree. Pass `--dry-run` to preview without changing anything.

```console
scripts/bump-version.sh
```

### APT repository

The `apt-repo.yml` workflow publishes the signed APT repository to GitHub Pages at `https://moritzheiber.github.io/keylightd`. It runs on every published release and on manual dispatch. It downloads the release `.deb` assets, builds a `resolute` archive for amd64 and arm64 with `reprepro`, signs the `Release` with the archive key, and deploys the result to Pages. The build is stateless and serves only the latest release, so a re-run fully reconstructs the site.

One-time setup:

- Enable Pages with the GitHub Actions source: `gh api --method POST repos/moritzheiber/keylightd/pages -f build_type=workflow`.
- Create a dedicated ed25519 signing key and store its private half as the `APT_GPG_PRIVATE_KEY` secret. The workflow verifies the imported key against the `EXPECTED_FPR` fingerprint pinned in `apt-repo.yml` (currently `64C33EBB1F9FE944F27B19B6C4296BDAD3AC2EDB`) and signs with it.

To rotate the key, generate a new one, replace the secret, update `EXPECTED_FPR` in `apt-repo.yml`, and re-run the workflow. Users pick up the new key from `keylightd.gpg` on the next `apt update`.
