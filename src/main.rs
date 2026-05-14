use std::time::Duration;
use anyhow::Result;
use bluer::{Address, AddressType};
use bluer::l2cap::{SeqPacket, SeqPacketListener, SocketAddr};
use tokio::time::{interval, timeout};

mod bluetooth;
mod protocol;

use bluetooth::BluetoothAdapter;
use protocol::{Handshake, Timer, idle_report};

const PSM_CTRL: u16 = 17;
const PSM_ITRP: u16 = 19;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    check_root();

    if !std::path::Path::new("/var/run/sdp").exists() {
        eprintln!(
            "Error: /var/run/sdp missing\n\
             BlueZ must run in compatibility mode:\n\
               bluetoothd -C --noplugin=input,sap,avrcp"
        );
        std::process::exit(1);
    }

    let switch_mac: Option<Address> = std::env::args()
        .nth(1)
        .map(|s| s.parse().expect("invalid MAC address"));

    let bt = BluetoothAdapter::new().await?;
    let adapter_addr = bt.address().await?;
    let addr_str = adapter_addr.to_string();

    loop {
        let conn = if let Some(mac) = switch_mac {
            reconnect(mac).await
        } else {
            connect_passive(&bt, adapter_addr).await
        };

        let (mut itrp, ctrl) = match conn {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("Connection error: {} — retrying in 2s", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let quit = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\nExiting");
                true
            }
            res = run_session(&mut itrp, ctrl, &addr_str) => match res {
                Ok(_)  => true,
                Err(e) => { eprintln!("Lost connection: {} — reconnecting", e); false }
            }
        };

        if quit { break; }
    }

    Ok(())
}

async fn connect_passive(bt: &BluetoothAdapter, addr: Address) -> Result<(SeqPacket, SeqPacket)> {
    // Bind sockets BEFORE set_discoverable — setting discoverable resets the device class
    let ln_ctrl = SeqPacketListener::bind(SocketAddr { addr, addr_type: AddressType::BrEdr, psm: PSM_CTRL, cid: 0 }).await?;
    let ln_itrp = SeqPacketListener::bind(SocketAddr { addr, addr_type: AddressType::BrEdr, psm: PSM_ITRP, cid: 0 }).await?;

    // set_discoverable before set_device_class — class resets if set first
    bt.set_discoverable(true).await?;
    bt.set_device_class()?;

    println!("\nListening for Switch to connect to '{}' ...", addr);
    println!("On the Switch: Controllers → Change Grip/Order\n");

    let (itrp, peer) = ln_itrp.accept().await?;
    let (ctrl, _)    = ln_ctrl.accept().await?;

    println!("Switch connected from {}", peer.addr);
    Ok((itrp, ctrl))
}

async fn reconnect(mac: Address) -> Result<(SeqPacket, SeqPacket)> {
    println!("Reconnecting to Switch at {} ...", mac);
    let ctrl = SeqPacket::connect(SocketAddr { addr: mac, addr_type: AddressType::BrEdr, psm: PSM_CTRL, cid: 0 }).await?;
    let itrp = SeqPacket::connect(SocketAddr { addr: mac, addr_type: AddressType::BrEdr, psm: PSM_ITRP, cid: 0 }).await?;
    println!("Connected to Switch");
    Ok((itrp, ctrl))
}

async fn run_session(itrp: &mut SeqPacket, _ctrl: SeqPacket, addr_str: &str) -> Result<()> {
    run_handshake(itrp, addr_str).await?;
    idle_loop(itrp).await
}

async fn run_handshake(itrp: &mut SeqPacket, addr_str: &str) -> Result<()> {
    let mut hs = Handshake::new(addr_str);

    // Send initial idle report to prompt the Switch to start the handshake
    let init = hs.process(None);
    itrp.send(&init).await?;

    let mut received = false;
    while !hs.is_complete() {
        let mut buf = [0u8; 50];
        let n = match timeout(Duration::from_millis(1), itrp.recv(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => { received = true; Some(n) }
            _                  => None,
        };

        let reply = hs.process(n.map(|n| &buf[..n]));
        itrp.send(&reply).await?;

        if received {
            tokio::time::sleep(Duration::from_millis(1000 / 15)).await;
        } else {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    println!("Pairing complete — assigned player {}", hs.player_number.unwrap_or(0));
    Ok(())
}

async fn idle_loop(itrp: &mut SeqPacket) -> Result<()> {
    let mut timer  = Timer::new();
    let mut ticker = interval(Duration::from_micros(7576)); // ~132 Hz
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    println!("Entering idle loop (Ctrl-C to quit)");

    loop {
        ticker.tick().await;

        // Drain any incoming data
        let mut buf = [0u8; 50];
        let _ = timeout(Duration::from_micros(100), itrp.recv(&mut buf)).await;

        let msg = idle_report(&mut timer);
        itrp.send(&msg).await
            .map_err(|e| anyhow::anyhow!("Switch disconnected: {}", e))?;
    }
}

fn check_root() {
    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u32>().ok())
        })
        .unwrap_or(1);

    if uid != 0 {
        eprintln!("Error: must run as root (sudo ./controller)");
        std::process::exit(1);
    }
}
