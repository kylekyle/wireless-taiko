use std::ffi::CString;
use hidapi::HidApi;
use tokio::sync::watch;
use crate::protocol::ButtonState;

pub fn find_drums() -> Vec<String> {
    let Ok(api) = HidApi::new() else { return vec![]; };
    api.device_list()
        .filter(|d| d.product_string().is_some_and(|s| s.contains("Taiko")))
        .map(|d| d.path().to_string_lossy().into_owned())
        .collect()
}

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
            Ok(0) => {}  // timeout, no new data
            Ok(_) => {
                let state = hid_to_button_state(&buf);
                if tx.send(state).is_err() { break; }
            }
            Err(e) => {
                eprintln!("[usb] {path} disconnected: {e}");
                break;
            }
        }
    }
}

fn hid_to_button_state(raw: &[u8]) -> ButtonState {
    if raw.len() < 3 { return ButtonState::default(); }

    let b0  = raw[0];
    let b1  = raw[1];
    let hat = raw[2];

    let mut right:  u8 = 0;
    let mut shared: u8 = 0;
    let mut left:   u8 = 0;

    // byte 0 button bits
    if b0 & 0x01 != 0 { right  |= 0x01; } // Y
    if b0 & 0x02 != 0 { right  |= 0x04; } // B
    if b0 & 0x04 != 0 { right  |= 0x08; } // A
    if b0 & 0x08 != 0 { right  |= 0x02; } // X
    if b0 & 0x10 != 0 { left   |= 0x08; } // DPAD_LEFT
    if b0 & 0x20 != 0 { left   |= 0x04; } // DPAD_RIGHT
    if b0 & 0x40 != 0 { left   |= 0x08; } // left ka  → DPAD_LEFT
    if b0 & 0x80 != 0 { right  |= 0x08; } // right ka → A

    // byte 1 button bits
    if b1 & 0x01 != 0 { shared |= 0x01; } // MINUS
    if b1 & 0x02 != 0 { shared |= 0x02; } // PLUS
    if b1 & 0x04 != 0 { left   |= 0x01; } // left dom  → DOWN
    if b1 & 0x08 != 0 { right  |= 0x04; } // right dom → B
    if b1 & 0x10 != 0 { shared |= 0x10; } // HOME
    if b1 & 0x20 != 0 { shared |= 0x20; } // CAPTURE

    // hat switch (d-pad); idle value = 0x0F (no press)
    match hat {
        0 => left |= 0x02, // UP
        2 => left |= 0x04, // RIGHT
        4 => left |= 0x01, // DOWN
        6 => left |= 0x08, // LEFT
        _ => {}
    }

    ButtonState { right, shared, left }
}
