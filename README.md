# keylightd

Turns an Elgato Key Light on when any V4L2 camera produces frames, chooses brightness from ambient-light calibration, and restores the previous power and brightness state five seconds after capture stops.

See [SPEC.md](SPEC.md) for behavioral decisions, architecture, edge cases, and extension guidance.

## Install

Download the package for your architecture from a GitHub release, then install it:

```console
sudo apt install ./keylightd_0.1.0-1_amd64.deb
```

Installation enables and starts the privileged camera service and enables the per-user controller globally, so the controller starts in the current graphical session and on each subsequent login.

Configuration is optional: with no config file the daemon uses defaults and auto-selects a single discovered Key Light. To start from the documented example, copy it into place and reload:

```console
install -Dm 0644 /usr/share/doc/keylightd/config.example.toml ~/.config/keylightd/config.toml
keylightd reload
```

The privileged service reads only the kernel V4L2 dequeue tracepoint and publishes a heartbeat plus per-camera frame state under `/run/keylightd`. The user service owns device selection, Key Light control, and restoration.

## Build a package

The local builder requires only Docker with Buildx. The Rust toolchain is pinned by `rust-toolchain.toml` and installed with `rustup` inside the Ubuntu 26.04 build stage, where compilation, cross-compilers, and `dpkg-deb` run:

```console
scripts/build-deb.sh
sudo apt install ./dist/keylightd_0.1.0-1_amd64.deb
```

Use `--architecture arm64` to cross-compile an arm64 package. Pushing a tag matching the Cargo version, such as `v0.1.0`, runs the same containerized builder for amd64 and arm64 and attaches both packages to a GitHub release.

## Configure

Use `keylightd lights list` and `keylightd lights select` to choose one or more lights. Use `keylightd cameras list` and `keylightd cameras select` to restrict camera activation. Without an explicit camera selection, all detected capture cameras are included.

`keylightd sensor` prints the current IIO ambient-light reading. Run `keylightd calibrate` under several lighting conditions to save lux-to-brightness points; intermediate values are linearly interpolated. Without a sensor or calibration, camera activation preserves the Key Light's existing brightness.

Run `keylightd reload` after editing configuration. Set `RUST_LOG=keylightd=debug` for additional journal output.

## Limitations

Key Lights are controlled over the unencrypted Elgato local API on port `9123`. No TLS backend is compiled in, so only `http://` endpoints work; `https://` endpoints are rejected. This matches the devices, which expose no HTTPS listener, and keeps the release binary small by avoiding an asynchronous runtime and bundled cryptography libraries.
