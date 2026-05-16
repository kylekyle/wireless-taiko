// USB Taiko drum input via HID.
//
// The Taiko drum presents as a standard USB HID gamepad. We read 8-byte
// reports and translate the drum zone bits to Nintendo Switch Pro Controller
// button bytes (r[4]/r[5]/r[6] in the 0x30 input report).
//
// HID report layout (8 bytes):
//   byte 0 — face/drum buttons
//             bit 0: Y           bit 1: B           bit 2: A        bit 3: X
//             bit 4: DPAD_LEFT   bit 5: DPAD_RIGHT
//             bit 6: left rim (ka) → L              bit 7: right rim (ka) → R
//   byte 1 — menu + extra drum buttons
//             bit 0: MINUS       bit 1: PLUS
//             bit 2: left center (don) → B          bit 3: right center (don) → A
//             bit 4: HOME        bit 5: CAPTURE
//   byte 2 — hat switch (d-pad encoded as compass direction)
//             0=Up  2=Right  4=Down  6=Left  0x0F=idle (no press)
//   bytes 3–7 — unused (analog axes, always mid-scale)

use std::ffi::CString;
use hidapi::HidApi;
use tokio::sync::watch;
use crate::protocol::ButtonState;

// Returns hidraw paths (e.g. "/dev/hidraw0") for all connected Taiko drums.
// Called every second from the main polling loop; creates a fresh HidApi each
// call so newly plugged devices are discovered without persistent state.
pub fn find_drums() -> Vec<String> {
    let Ok(api) = HidApi::new() else { return vec![]; };
    api.device_list()
        .filter(|d| d.product_string().is_some_and(|s| s.contains("Taiko")))
        .map(|d| d.path().to_string_lossy().into_owned())
        .collect()
}

// Blocking HID read loop — intended for tokio::task::spawn_blocking.
//
// Reads 8-byte reports from the drum and translates each to a ButtonState,
// which is sent over a watch channel. watch semantics mean the async side
// always reads the *latest* state with no queue buildup — correct here since
// we only care about the current drum state, not every intermediate report.
//
// When the function returns (on I/O error or tx.send failure), it drops tx.
// The controller task detects this via rx.has_changed().is_err() and exits.
pub fn read_drum(path: String, tx: watch::Sender<ButtonState>) {
    let api = match HidApi::new() {
        Ok(a) => a,
        Err(e) => { eprintln!("[usb] HidApi init failed: {e}"); return; }
    };
    let cpath = match CString::new(path.as_bytes()) {
        Ok(c) => c,
        Err(e) => { eprintln!("[usb] invalid path {path}: {e}"); return; }
    };
    let dev = match api.open_path(&cpath) {
        Ok(d) => d,
        Err(e) => { eprintln!("[usb] failed to open {path}: {e}"); return; }
    };

    let mut buf = [0u8; 8];
    loop {
        match dev.read_timeout(&mut buf, 100) {
            // 100ms timeout elapsed with no data — loop and try again.
            // The short timeout means we notice tx drop (drum unplugged from
            // the async side) within 100ms rather than blocking indefinitely.
            Ok(0) => {}
            Ok(_) => {
                let state = hid_to_button_state(&buf);
                // tx.send fails only when all receivers are dropped, meaning
                // the controller task has already exited.
                if tx.send(state).is_err() { break; }
            }
            Err(e) => {
                eprintln!("[usb] {path} disconnected: {e}");
                break;
            }
        }
    }
}

// Translates a raw 8-byte Taiko HID report into a Pro Controller ButtonState.
//
// Pro Controller button byte layout (see protocol.rs ButtonState):
//   right  (r[4]): Y=0x01  X=0x02  B=0x04  A=0x08  R=0x40  ZR=0x80
//   shared (r[5]): Minus=0x01  Plus=0x02  Home=0x10  Capture=0x20
//   left   (r[6]): Down=0x01  Up=0x02  Right=0x04  Left=0x08  L=0x40  ZL=0x80
fn hid_to_button_state(raw: &[u8]) -> ButtonState {
    if raw.len() < 3 { return ButtonState::default(); }

    let b0  = raw[0];
    let b1  = raw[1];
    let hat = raw[2];

    let mut right:  u8 = 0;
    let mut shared: u8 = 0;
    let mut left:   u8 = 0;

    // byte 0 — face buttons and drum left/right rim (ka)
    if b0 & 0x01 != 0 { right  |= 0x01; } // Y
    if b0 & 0x02 != 0 { right  |= 0x04; } // B
    if b0 & 0x04 != 0 { right  |= 0x08; } // A
    if b0 & 0x08 != 0 { right  |= 0x02; } // X
    if b0 & 0x10 != 0 { left   |= 0x08; } // DPAD_LEFT
    if b0 & 0x20 != 0 { left   |= 0x04; } // DPAD_RIGHT
    if b0 & 0x40 != 0 { left   |= 0x40; } // left ka  → L
    if b0 & 0x80 != 0 { right  |= 0x40; } // right ka → R

    // byte 1 — menu buttons and drum center (don)
    if b1 & 0x01 != 0 { shared |= 0x01; } // MINUS
    if b1 & 0x02 != 0 { shared |= 0x02; } // PLUS
    if b1 & 0x04 != 0 { right  |= 0x04; } // left don  → B
    if b1 & 0x08 != 0 { right  |= 0x08; } // right don → A
    if b1 & 0x10 != 0 { shared |= 0x10; } // HOME
    if b1 & 0x20 != 0 { shared |= 0x20; } // CAPTURE

    // byte 2 — hat switch; 0x0F when no direction pressed
    match hat {
        0 => left |= 0x02, // UP
        2 => left |= 0x04, // RIGHT
        4 => left |= 0x01, // DOWN
        6 => left |= 0x08, // LEFT
        _ => {}
    }

    ButtonState { right, shared, left }
}
