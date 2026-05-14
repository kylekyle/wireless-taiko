# wireless-taiko

Makes USB Taiko drums wireless on the Nintendo Switch using a Raspberry Pi. The Pi advertises itself as a Pro Controller over Bluetooth and forwards drum hits as button presses.

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

**2. SDP socket permissions**

BlueZ's SDP socket must be world-writable. Add this to `/etc/rc.local` (before `exit 0`) so it survives reboots:

```bash
chmod 777 /var/run/sdp
```

**3. Unblock the radio**

```bash
sudo rfkill unblock all
```

## Dev machine setup

You need [cross](https://github.com/cross-rs/cross) and [sccache](https://github.com/mozilla/sccache) installed, and Docker running.

```bash
cargo install cross sccache
```

## Build and deploy

```bash
cargo deploy
```

This cross-compiles for `aarch64-unknown-linux-gnu` and SCPs the binary to `pi@dom.local:~/rust/pro-controller`.

## Run on the Pi

```bash
ssh pi@dom.local sudo ~/rust/pro-controller
```

On the Switch: **Controllers → Change Grip/Order**. The Switch will find the adapter advertising as "Pro Controller" and pair automatically.
