# keylightd

Turns an Elgato Key Light on when any V4L2 camera produces frames, chooses brightness from ambient-light calibration, and restores the previous power and brightness state five seconds after capture stops.

See [SPEC.md](SPEC.md) for behavioral decisions, architecture, edge cases, and extension guidance.

## Install

Download the package for your architecture from a GitHub release, then install it:

```console
sudo apt install ./keylightd_0.1.0-1_amd64.deb
install -Dm 0644 /usr/share/doc/keylightd/config.example.toml ~/.config/keylightd/config.toml
systemctl --user daemon-reload
systemctl --user enable --now keylightd.service
```

The package enables the privileged camera service. The user service is enabled separately for the desired graphical account.

The privileged service reads only the kernel V4L2 dequeue tracepoint and publishes a heartbeat plus per-camera frame state under `/run/keylightd`. The user service owns device selection, Key Light control, and restoration.

## Build a package

The local builder requires only Docker with Buildx. Rust comes from the official image; compilation, cross-compilers, and `dpkg-deb` run inside Ubuntu 26.04:

```console
scripts/build-deb.sh
sudo apt install ./dist/keylightd_0.1.0-1_amd64.deb
```

Use `--architecture arm64` to cross-compile an arm64 package. Pushing a tag matching the Cargo version, such as `v0.1.0`, runs the same containerized builder for amd64 and arm64 and attaches both packages to a GitHub release.

## Configure

Use `keylightd lights list` and `keylightd lights select` to choose one or more lights. Use `keylightd cameras list` and `keylightd cameras select` to restrict camera activation. Without an explicit camera selection, all detected capture cameras are included.

`keylightd sensor` prints the current IIO ambient-light reading. Run `keylightd calibrate` under several lighting conditions to save lux-to-brightness points; intermediate values are linearly interpolated. Without a sensor or calibration, camera activation preserves the Key Light's existing brightness.

Run `keylightd reload` after editing configuration. Set `RUST_LOG=keylightd=debug` for additional journal output.
