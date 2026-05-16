# wireless-taiko

Makes USB Taiko drums wireless on the Nintendo Switch using a Raspberry Pi. The Pi advertises itself as a Pro Controller over Bluetooth and forwards drum hits as button presses.

You need one Bluetooth adapter per drum — each drum pairs as an independent controller. The Raspberry Pi 4 has one built-in adapter; add USB Bluetooth dongles for additional drums.

## Raspberry Pi setup

Tested on Raspberry Pi 4 Model B running Raspberry Pi OS Lite (64-bit).

**1. BlueZ compatibility mode**

Edit `/lib/systemd/system/bluetooth.service` and change the `ExecStart` line:

```
ExecStart=/usr/libexec/bluetooth/bluetoothd -C --noplugin=input,sap,avrcp
```

Then reload and restart:

```bash
sudo systemctl daemon-reload
sudo systemctl restart bluetooth
```

**2. Disable Secure Connections**

Edit `/etc/bluetooth/main.conf` and set:

```
[Policy]
SecureConnections = off
```

Without this, BlueZ escalates SSP to Numeric Comparison even when both sides advertise `NoInputNoOutput`, and pairing fails.

Restart Bluetooth after changing the config:

```bash
sudo systemctl restart bluetooth
```

**3. SDP socket permissions**

BlueZ's SDP socket must be world-writable. Add this to `/etc/rc.local` (before `exit 0`) so it survives reboots:

```bash
chmod 777 /var/run/sdp
```

**4. Unblock the radio**

```bash
sudo rfkill unblock all
```

## Install

Download the latest `wireless-taiko` binary from the [Releases](https://github.com/kylekyle/wireless-taiko/releases) page and copy it to the Pi:

```bash
scp wireless-taiko pi@raspberrypi.local:~/
ssh pi@raspberrypi.local chmod +x ~/wireless-taiko
```

## Run as a service

Download `wireless-taiko.service` from the [Releases](https://github.com/kylekyle/wireless-taiko/releases) page (or copy it from the repo) and install it:

```bash
sudo cp wireless-taiko.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now wireless-taiko
```

The service starts on boot, restarts automatically on failure, and logs to journald:

```bash
journalctl -u wireless-taiko -f
```

To update the binary, stop the service first — the file cannot be replaced while the process is running:

```bash
sudo systemctl stop wireless-taiko
# copy new binary
sudo systemctl start wireless-taiko
```

## Build from source

You need [cross](https://github.com/cross-rs/cross) and [sccache](https://github.com/mozilla/sccache) installed, and Docker running:

```bash
cargo install cross sccache
```

Set `TAIKO_HOST` to your Pi's SSH target, then run:

```bash
export TAIKO_HOST=pi@raspberrypi.local
cargo deploy
```

This cross-compiles for `aarch64-unknown-linux-gnu` and SCPs the binary to `$TAIKO_HOST:~/wireless-taiko`. Override the remote path with `TAIKO_REMOTE_PATH` if needed.

## Run manually

```bash
ssh pi@raspberrypi.local sudo ~/wireless-taiko
```

On the Switch: **Controllers → Change Grip/Order**. The Switch will find the adapter advertising as "Pro Controller" and pair automatically.
