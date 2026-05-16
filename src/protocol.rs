// Nintendo Switch Pro Controller protocol over Bluetooth Classic (BR/EDR).
//
// The Switch is the HID *host*; we are the HID *device* (the controller).
// All traffic flows on PSM 19 (HID Interrupt). PSM 17 (HID Control) is bound
// to satisfy the SDP record but the Switch never sends meaningful data there.
//
// Packet framing (first byte identifies direction and type):
//   0xA1  HID DATA       — input report, device → host (us → Switch)
//   0xA2  HID SET_REPORT — output report, host → device (Switch → us)
//
// Input report IDs (byte 1 after 0xA1):
//   0x21  Subcommand reply  — sent in response to every 0xA2 subcommand
//   0x30  Standard full     — periodic input report sent during normal operation
//
// Handshake sequence (Switch always initiates):
//   Switch sends 0xA2 with subcommand byte at offset 10 (0-indexed):
//     0x02  RequestDeviceInfo     — firmware, controller type, MAC
//     0x08  SetShipmentLowPower   — ACK only
//     0x10  SPIRead               — read calibration flash (many rounds)
//     0x03  SetInputReportMode    — switch to standard 0x30 reports
//     0x04  TriggerButtonsElapsed — ACK only
//     0x40  EnableIMU             — ACK only
//     0x48  EnableVibration       — ACK; marks us as vibration-capable
//     0x22  SetNFCIRState         — ACK only (we have no NFC/IR)
//     0x21  SetNFCIRConfig        — specific reply telling Switch we have no NFC/IR
//     0x30  SetPlayerLights       — encodes player slot; ACK; marks handshake complete
//
// The Switch does not require these in strict order — we just respond to
// whatever arrives. Handshake is complete once vibration is enabled and the
// player number has been assigned (0x30 SET_PLAYER_LIGHTS received).

use std::time::Instant;

// The three button bytes in a standard 0x30 input report.
// These map directly to bytes r[4], r[5], r[6] of the report.
#[derive(Clone, Default, PartialEq)]
pub struct ButtonState {
    pub right:  u8,  // r[4]: Y=0x01 X=0x02 B=0x04 A=0x08 R=0x40 ZR=0x80
    pub shared: u8,  // r[5]: Minus=0x01 Plus=0x02 Home=0x10 Capture=0x20
    pub left:   u8,  // r[6]: Down=0x01 Up=0x02 Right=0x04 Left=0x08 L=0x40 ZL=0x80
}

// Subcommand IDs sent by the Switch in 0xA2 output reports.
const CMD_DEVICE_INFO:      u8 = 0x02;
const CMD_SET_SHIPMENT:     u8 = 0x08;
const CMD_SPI_READ:         u8 = 0x10;
const CMD_SET_MODE:         u8 = 0x03;
const CMD_TRIGGER_BUTTONS:  u8 = 0x04;
const CMD_TOGGLE_IMU:       u8 = 0x40;
const CMD_ENABLE_VIBRATION: u8 = 0x48;
const CMD_SET_PLAYER:       u8 = 0x30;
const CMD_SET_NFC_IR_STATE:  u8 = 0x22;
const CMD_SET_NFC_IR_CONFIG: u8 = 0x21;

// Vibration byte values cycled round-robin in every report.
// The Switch monitors that this byte keeps changing to confirm the controller
// is alive and the vibration motor driver is functional. A stuck value causes
// the Switch to treat the controller as frozen and drop the connection.
const VIBRATOR_BYTES: [u8; 4] = [0xA0, 0xB0, 0xC0, 0x90];

// Stick center positions encoded as three bytes per stick.
// The Pro Controller encodes each axis as a 12-bit value; two axes share three
// bytes: byte[0]=lo8(x), byte[1]=hi4(x)|lo4(y)<<4, byte[2]=hi8(y).
// These constants place both sticks at mechanical center (~2048 out of 4095).
const LEFT_STICK:  [u8; 3] = [0x6F, 0xC8, 0x77];
const RIGHT_STICK: [u8; 3] = [0x16, 0xD8, 0x7D];

// Report timestamp and vibration state, shared across all reports.
pub struct Timer {
    value:   u8,
    last:    Option<Instant>,
    vib_idx: u8,
}

impl Timer {
    pub fn new() -> Self {
        Self { value: 0, last: None, vib_idx: 0 }
    }

    // Advances the report timestamp by elapsed time × 4 (units per ms).
    // The u8 wraps freely; the Switch only checks that it increments, not its
    // absolute value. ~4 units/ms → full cycle every ~64 ms at 60 Hz.
    fn tick(&mut self) -> u8 {
        let now = Instant::now();
        if let Some(last) = self.last.replace(now) {
            let delta_ms = now.duration_since(last).as_secs_f64() * 1000.0;
            self.value = self.value.wrapping_add((delta_ms * 4.0) as u8);
        }
        self.value
    }

    fn vib(&mut self) -> u8 {
        let b = VIBRATOR_BYTES[self.vib_idx as usize];
        self.vib_idx = (self.vib_idx + 1) % 4;
        b
    }
}

// Allocates a zeroed 50-byte buffer and sets the HID DATA header byte.
fn make_report() -> [u8; 50] {
    let mut r = [0u8; 50];
    r[0] = 0xA1; // HID DATA — input report, device → host
    r
}

// Fills the fields common to both 0x21 and 0x30 reports:
//   r[2]     timestamp
//   r[3]     0x90: battery full (high nibble) + Pro Controller via BT (low nibble)
//   r[7..10] left stick center
//   r[10..13] right stick center
//   r[13]    vibration cycling byte
fn fill_standard(r: &mut [u8; 50], timer: &mut Timer) {
    r[2]  = timer.tick();
    r[3]  = 0x90;
    r[7]  = LEFT_STICK[0];
    r[8]  = LEFT_STICK[1];
    r[9]  = LEFT_STICK[2];
    r[10] = RIGHT_STICK[0];
    r[11] = RIGHT_STICK[1];
    r[12] = RIGHT_STICK[2];
    r[13] = timer.vib();
}

pub fn idle_report(timer: &mut Timer) -> [u8; 50] {
    input_report(timer, &ButtonState::default())
}

// Builds a 0x30 standard full input report with current button state.
// Sent at ~60 Hz during normal operation to keep the Switch connection alive.
pub fn input_report(timer: &mut Timer, state: &ButtonState) -> [u8; 50] {
    let mut r = make_report();
    r[1] = 0x30; // report ID: standard full input report
    fill_standard(&mut r, timer);
    r[4] = state.right;
    r[5] = state.shared;
    r[6] = state.left;
    r
}

// State machine for the pairing handshake.
pub struct Handshake {
    bt_addr:    [u8; 6],  // adapter MAC, included in the device info reply
    body_color: [u8; 3],  // RGB body color reported to the Switch
    timer:      Timer,
    pub vibration_enabled: bool,  // set when 0x48 ENABLE_VIBRATION is received
    pub player_number:     Option<u8>, // set when 0x30 SET_PLAYER_LIGHTS is received
}

impl Handshake {
    // Derives a unique body color from the adapter MAC so multiple controllers
    // are visually distinct on-screen without a random-number generator.
    pub fn new(bt_addr_str: &str) -> Self {
        let mut bt_addr = [0u8; 6];
        for (i, part) in bt_addr_str.split(':').enumerate().take(6) {
            bt_addr[i] = u8::from_str_radix(part, 16).unwrap_or(0);
        }
        let hue = bt_addr[3].wrapping_add(bt_addr[4]).wrapping_add(bt_addr[5]);
        let body_color = hue_to_rgb(hue);
        Self { bt_addr, body_color, timer: Timer::new(), vibration_enabled: false, player_number: None }
    }

    // Handshake is done once both sides have agreed on vibration and the Switch
    // has assigned a player slot. After this point we switch to pure 0x30 input
    // reports and the semaphore is released to let the next adapter pair.
    pub fn is_complete(&self) -> bool {
        self.vibration_enabled && self.player_number.is_some()
    }

    // Feed an incoming packet (or None for the boot report) and get the reply.
    // None / short / non-0xA2 packets all return a bare input report because
    // the Switch expects reports to keep flowing even between subcommands.
    pub fn process(&mut self, pkt: Option<&[u8]>) -> [u8; 50] {
        let data = match pkt {
            None                     => return idle_report(&mut self.timer),
            Some(d) if d.len() < 50 => return idle_report(&mut self.timer),
            Some(d) if d[0] != 0xA2 => return idle_report(&mut self.timer),
            Some(d)                  => d,
        };

        // Subcommand byte is at offset 10 within the 0xA2 output report.
        let sub = &data[11..];
        match sub[0] {
            CMD_DEVICE_INFO      => self.reply_device_info(),
            CMD_SET_SHIPMENT     => self.reply_ack(0x80, 0x08),
            CMD_SPI_READ         => { let r = self.reply_base(); spi_reply(r, sub, &self.body_color) }
            CMD_SET_MODE         => self.reply_ack(0x80, 0x03),
            CMD_TRIGGER_BUTTONS  => self.reply_ack(0x83, 0x04),
            CMD_TOGGLE_IMU       => self.reply_ack(0x80, 0x40),
            CMD_ENABLE_VIBRATION => { self.vibration_enabled = true; self.reply_ack(0x82, 0x48) }
            CMD_SET_PLAYER       => { self.set_player(sub[1]); self.reply_ack(0x80, 0x30) }
            CMD_SET_NFC_IR_STATE  => self.reply_ack(0x80, 0x22),
            CMD_SET_NFC_IR_CONFIG => self.reply_nfc_ir_config(),
            _                    => idle_report(&mut self.timer),
        }
    }

    // 0x21 subcommand reply header (report ID + standard fields).
    fn reply_base(&mut self) -> [u8; 50] {
        let mut r = make_report();
        r[1] = 0x21; // report ID: subcommand reply
        fill_standard(&mut r, &mut self.timer);
        r
    }

    // Generic ACK: r[14]=ack byte, r[15]=echoed subcommand ID.
    fn reply_ack(&mut self, ack: u8, sub: u8) -> [u8; 50] {
        let mut r = self.reply_base();
        r[14] = ack;
        r[15] = sub;
        r
    }

    // 0x02 RequestDeviceInfo — Switch needs firmware version, controller type,
    // and MAC address before it will proceed with the rest of the handshake.
    fn reply_device_info(&mut self) -> [u8; 50] {
        let mut r = self.reply_base();
        r[14] = 0x82; r[15] = 0x02;        // ACK for subcommand 0x02
        r[16] = 0x03; r[17] = 0x8B;        // firmware version 3.139 (any value works)
        r[18] = 0x03; r[19] = 0x02;        // controller type 0x03 = Pro Controller
        r[20..26].copy_from_slice(&self.bt_addr); // MAC address (6 bytes, as-is)
        r[26] = 0x01; r[27] = 0x01;        // fixed: "colours in SPI flash"
        r
    }

    // 0x21 SetNFCIRMCUConfig — Switch asks this even for controllers without
    // NFC/IR. The specific payload tells the Switch the MCU is not present.
    fn reply_nfc_ir_config(&mut self) -> [u8; 50] {
        let mut r = self.reply_base();
        r[14] = 0xA0; r[15] = 0x21;
        r[16..24].copy_from_slice(&[0x01, 0x00, 0xFF, 0x00, 0x08, 0x00, 0x1B, 0x01]);
        r[49] = 0xC8; // trailing byte the Switch validates
        r
    }

    // 0x30 SetPlayerLights — bit pattern encodes the player slot.
    // Both 0x01 and 0x10 mean player 1 (solid vs blinking LED).
    fn set_player(&mut self, bits: u8) {
        self.player_number = match bits {
            0x01 | 0x10 => Some(1),
            0x03 | 0x30 => Some(2),
            0x07 | 0x70 => Some(3),
            0x0F | 0xF0 => Some(4),
            _           => None,
        };
    }
}

// 0x10 SPIRead — Switch reads calibration and color data from the controller's
// SPI flash. We return hardcoded blobs keyed by (hi byte, lo byte) of address:
//
//   0x6000  User analog stick calibration (0xFF = uncalibrated, use factory)
//   0x6050  Controller body/button/grip colors
//   0x6080  Factory left-stick calibration + 6-axis sensor factory calibration
//   0x6098  Factory right-stick calibration
//   0x8010  Serial number area (0xFF = blank)
//   0x603D  Factory analog stick calibration (neutral, min, max per axis)
//   0x6020  6-axis sensor factory calibration data
fn spi_reply(mut r: [u8; 50], sub: &[u8], color: &[u8; 3]) -> [u8; 50] {
    let lo  = sub[1];
    let hi  = sub[2];
    let len = sub[5];
    r[14] = 0x90; r[15] = 0x10; // ACK for 0x10 SPI_READ
    r[16] = lo;   r[17] = hi;   // echo the requested address
    r[20] = len;                 // echo the requested length

    // Factory calibration blob for 0x6080 / 0x6098 (sensor + stick cal).
    const SP: [u8; 18] = [
        0x0F, 0x30, 0x61, 0x96, 0x30, 0xF3,
        0xD4, 0x14, 0x54, 0x41, 0x15, 0x54,
        0xC7, 0x79, 0x9C, 0x33, 0x36, 0x63,
    ];

    match (hi, lo) {
        // 0x6000: user calibration — all 0xFF means "use factory defaults"
        (0x60, 0x00) => r[21..37].fill(0xFF),

        // 0x6050: colors — body RGB, button RGB (white), grip RGB (none)
        (0x60, 0x50) => {
            r[21..24].copy_from_slice(color);
            r[24..27].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
            r[27..34].fill(0xFF);
        }

        // 0x6080: factory sensor calibration prefix + SP blob
        (0x60, 0x80) => {
            r[21..27].copy_from_slice(&[0x50, 0xFD, 0x00, 0x00, 0xC6, 0x0F]);
            r[27..45].copy_from_slice(&SP);
        }

        // 0x6098: factory right-stick calibration
        (0x60, 0x98) => r[21..39].copy_from_slice(&SP),

        // 0x8010: serial number / device info (blank)
        (0x80, 0x10) => r[21..45].fill(0xFF),

        // 0x603D: factory analog stick calibration (real Pro Controller values)
        (0x60, 0x3D) => {
            r[21..30].copy_from_slice(&[0xBA, 0xF5, 0x62, 0x6F, 0xC8, 0x77, 0xED, 0x95, 0x5B]);
            r[30..39].copy_from_slice(&[0x16, 0xD8, 0x7D, 0xF2, 0xB5, 0x5F, 0x86, 0x65, 0x5E]);
            r[39] = 0xFF;
            r[40..43].copy_from_slice(color);
            r[43..46].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        }

        // 0x6020: 6-axis sensor calibration data
        (0x60, 0x20) => r[21..45].copy_from_slice(&[
            0xD3, 0xFF, 0xD5, 0xFF, 0x55, 0x01,
            0x00, 0x40, 0x00, 0x40, 0x00, 0x40,
            0x19, 0x00, 0xDD, 0xFF, 0xDC, 0xFF,
            0x3B, 0x34, 0x3B, 0x34, 0x3B, 0x34,
        ]),
        _ => {}
    }
    r
}

// Maps a hue byte (0–255) to a fully-saturated, full-brightness RGB color.
// Used to give each controller a unique on-screen body color derived from
// the last three bytes of its Bluetooth MAC address.
fn hue_to_rgb(hue: u8) -> [u8; 3] {
    let h6     = hue as u32 * 6;
    let sector = (h6 / 256) as u8;
    let f      = (h6 % 256) as u8;
    let inv    = 255 - f;
    match sector {
        0 => [255, f,   0  ],
        1 => [inv, 255, 0  ],
        2 => [0,   255, f  ],
        3 => [0,   inv, 255],
        4 => [f,   0,   255],
        _ => [255, 0,   inv],
    }
}
