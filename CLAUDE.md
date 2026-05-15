# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust binary that runs on a Raspberry Pi and makes USB Taiko drums wireless on the Nintendo Switch. The Pi advertises itself as a Pro Controller over Bluetooth (BR/EDR, not BLE) and handles the full Switch pairing handshake.

## Build and deploy

The binary cross-compiles for `aarch64-unknown-linux-gnu` via Docker and is deployed over SSH. Docker must be running.

```bash
TAIKO_HOST=pi@dom.local cargo deploy   # build + scp to Pi
ssh pi@dom.local sudo ~/wireless-taiko # run on Pi (requires root for BT sockets)
```

`cargo deploy` is an alias (`.cargo/config.toml`) that runs the `xtask` crate. `Cross.toml` configures the Docker build container — it installs `libdbus-1-dev:arm64` (needed to link `bluer`) and `sccache`.

There are no tests and no local `cargo build` target — the binary only runs on Linux/ARM with BlueZ.

## Cross-compilation dependencies

`Cross.toml` installs these arm64 packages in the Docker build container:
- `libdbus-1-dev:arm64` — needed to link `bluer`
- `libudev-dev:arm64` — needed to link `hidapi`

The Pi also needs `libudev1` at runtime (installed by default on Raspberry Pi OS).

## Architecture

Four source files, no external state, single binary:

- **`src/bluetooth.rs`** — `BluetoothManager` creates one shared `bluer` session and registers the HID SDP profile once (BlueZ only allows one profile registration per session). `setup_adapter(name)` configures an individual adapter and returns a `ControllerAdapter`. SDP cleanup (`sdptool`) and profile registration happen once at startup; per-adapter setup (alias, pairable, stale-pairing removal) happens per controller task.

- **`src/protocol.rs`** — all Switch wire protocol. `Handshake` processes the 8-step pairing sequence. `Timer` maintains the report timestamp counter. `ButtonState` holds the 3 button bytes (right/shared/left). `input_report(timer, state)` builds the 50-byte `0x30` input report. No I/O — pure byte manipulation.

- **`src/usb.rs`** — USB Taiko drum support. `find_drums()` enumerates connected devices via `hidapi`, filtering by product string containing `"Taiko"`. `read_drum(path, tx)` runs in a blocking thread, reading 8-byte HID reports and translating them to `ButtonState` via a `tokio::sync::watch` channel. Drum disconnect is detected when `read_drum` exits and drops `tx`, causing `rx.has_changed()` to return `Err`.

- **`src/main.rs`** — async entry point (single-threaded tokio + `LocalSet`). Polls for USB drums every second. For each new drum, claims a BT adapter from the pool (`Arc<Mutex<Vec<String>>>`) and spawns a `controller_task` via `spawn_local`. Each task: sets up its adapter, spawns a blocking USB reader thread, then loops — binding L2CAP sockets, accepting a Switch connection, running the handshake, entering the idle loop. On Switch disconnect the task re-advertises; on drum unplug the task exits and returns the adapter to the pool.

## Key constraints

- **Root required** — L2CAP `AF_BLUETOOTH` sockets need root on Linux.
- **Socket bind order** — bind L2CAP sockets before `set_discoverable`; BlueZ resets the device class when discoverable is toggled.
- **Discoverable before device class** — `set_discoverable` before `set_device_class` (hciconfig); class resets if set first.
- **PSM 19 drives the handshake** — all subcommand traffic flows on the interrupt channel (PSM 19), not the control channel (PSM 17). PSM 17 is bound but otherwise ignored.
- **BlueZ must run in compatibility mode** — `bluetoothd -C --noplugin=input,sap,avrcp`. See README for Pi setup.

## Python reference

`minimal/` is a working Python implementation of the same protocol (zero pip deps, uses system `python3-dbus`). Useful as a reference when debugging wire protocol issues. Not deployed.
