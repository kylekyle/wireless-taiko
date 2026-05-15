use std::time::Instant;

#[derive(Clone, Default)]
pub struct ButtonState {
    pub right:  u8,  // r[4]: Y=0x01 X=0x02 B=0x04 A=0x08 R=0x40 ZR=0x80
    pub shared: u8,  // r[5]: Minus=0x01 Plus=0x02 Home=0x10 Capture=0x20
    pub left:   u8,  // r[6]: Down=0x01 Up=0x02 Right=0x04 Left=0x08 L=0x40 ZL=0x80
}

const CMD_DEVICE_INFO: u8      = 0x02;
const CMD_SET_SHIPMENT: u8     = 0x08;
const CMD_SPI_READ: u8         = 0x10;
const CMD_SET_MODE: u8         = 0x03;
const CMD_TRIGGER_BUTTONS: u8  = 0x04;
const CMD_TOGGLE_IMU: u8       = 0x40;
const CMD_ENABLE_VIBRATION: u8 = 0x48;
const CMD_SET_PLAYER: u8       = 0x30;
const CMD_SET_NFC_IR_STATE: u8  = 0x22;
const CMD_SET_NFC_IR_CONFIG: u8 = 0x21;

const VIBRATOR_BYTES: [u8; 4] = [0xA0, 0xB0, 0xC0, 0x90];
const LEFT_STICK:  [u8; 3] = [0x6F, 0xC8, 0x77];
const RIGHT_STICK: [u8; 3] = [0x16, 0xD8, 0x7D];

pub struct Timer {
    value:   u8,
    last:    Option<Instant>,
    vib_idx: u8,
}

impl Timer {
    pub fn new() -> Self {
        Self { value: 0, last: None, vib_idx: 0 }
    }

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

fn make_report() -> [u8; 50] {
    let mut r = [0u8; 50];
    r[0] = 0xA1;
    r
}

fn fill_standard(r: &mut [u8; 50], timer: &mut Timer) {
    r[2]  = timer.tick();
    r[3]  = 0x90; // battery full, Pro Controller connection type
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

pub fn input_report(timer: &mut Timer, state: &ButtonState) -> [u8; 50] {
    let mut r = make_report();
    r[1] = 0x30;
    fill_standard(&mut r, timer);
    r[4] = state.right;
    r[5] = state.shared;
    r[6] = state.left;
    r
}

pub struct Handshake {
    bt_addr:    [u8; 6],
    body_color: [u8; 3],
    timer:      Timer,
    pub vibration_enabled: bool,
    pub player_number:     Option<u8>,
}

impl Handshake {
    pub fn new(bt_addr_str: &str) -> Self {
        let mut bt_addr = [0u8; 6];
        for (i, part) in bt_addr_str.split(':').enumerate().take(6) {
            bt_addr[i] = u8::from_str_radix(part, 16).unwrap_or(0);
        }
        // Derive a bright saturated color from the adapter's MAC address so each
        // controller gets a distinct color without a random number generator.
        let hue = bt_addr[3].wrapping_add(bt_addr[4]).wrapping_add(bt_addr[5]);
        let body_color = hue_to_rgb(hue);
        Self { bt_addr, body_color, timer: Timer::new(), vibration_enabled: false, player_number: None }
    }

    pub fn is_complete(&self) -> bool {
        self.vibration_enabled && self.player_number.is_some()
    }

    pub fn process(&mut self, pkt: Option<&[u8]>) -> [u8; 50] {
        let data = match pkt {
            None                     => return idle_report(&mut self.timer),
            Some(d) if d.len() < 50 => return idle_report(&mut self.timer),
            Some(d) if d[0] != 0xA2 => return idle_report(&mut self.timer),
            Some(d)                  => d,
        };

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

    fn reply_base(&mut self) -> [u8; 50] {
        let mut r = make_report();
        r[1] = 0x21;
        fill_standard(&mut r, &mut self.timer);
        r
    }

    fn reply_ack(&mut self, ack: u8, sub: u8) -> [u8; 50] {
        let mut r = self.reply_base();
        r[14] = ack;
        r[15] = sub;
        r
    }

    fn reply_device_info(&mut self) -> [u8; 50] {
        let mut r = self.reply_base();
        r[14] = 0x82; r[15] = 0x02;
        r[16] = 0x03; r[17] = 0x8B; // firmware 3.139
        r[18] = 0x03; r[19] = 0x02; // Pro Controller type, always 2
        r[20..26].copy_from_slice(&self.bt_addr);
        r[26] = 0x01; r[27] = 0x01; // always 1, colours in SPI
        r
    }

    fn reply_nfc_ir_config(&mut self) -> [u8; 50] {
        let mut r = self.reply_base();
        r[14] = 0xA0; r[15] = 0x21;
        r[16..24].copy_from_slice(&[0x01, 0x00, 0xFF, 0x00, 0x08, 0x00, 0x1B, 0x01]);
        r[49] = 0xC8;
        r
    }

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

fn spi_reply(mut r: [u8; 50], sub: &[u8], color: &[u8; 3]) -> [u8; 50] {
    let lo  = sub[1];
    let hi  = sub[2];
    let len = sub[5];
    r[14] = 0x90; r[15] = 0x10;
    r[16] = lo;   r[17] = hi;
    r[20] = len;

    const SP: [u8; 18] = [
        0x0F, 0x30, 0x61, 0x96, 0x30, 0xF3,
        0xD4, 0x14, 0x54, 0x41, 0x15, 0x54,
        0xC7, 0x79, 0x9C, 0x33, 0x36, 0x63,
    ];

    match (hi, lo) {
        (0x60, 0x00) => r[21..37].fill(0xFF),
        (0x60, 0x50) => {
            r[21..24].copy_from_slice(color);           // body
            r[24..27].copy_from_slice(&[0xFF, 0xFF, 0xFF]); // buttons: white
            r[27..34].fill(0xFF);                       // grips: none
        }
        (0x60, 0x80) => {
            r[21..27].copy_from_slice(&[0x50, 0xFD, 0x00, 0x00, 0xC6, 0x0F]);
            r[27..45].copy_from_slice(&SP);
        }
        (0x60, 0x98) => r[21..39].copy_from_slice(&SP),
        (0x80, 0x10) => r[21..45].fill(0xFF),
        (0x60, 0x3D) => {
            r[21..30].copy_from_slice(&[0xBA, 0xF5, 0x62, 0x6F, 0xC8, 0x77, 0xED, 0x95, 0x5B]);
            r[30..39].copy_from_slice(&[0x16, 0xD8, 0x7D, 0xF2, 0xB5, 0x5F, 0x86, 0x65, 0x5E]);
            r[39] = 0xFF;
            r[40..43].copy_from_slice(color);
            r[43..46].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        }
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
